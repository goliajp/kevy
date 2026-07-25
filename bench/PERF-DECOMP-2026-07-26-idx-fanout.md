# The IDX.QUERY gap is fan-out, and the engine underneath already beats PG

The PostgreSQL comparison ([PGCOMPARE-2026-07-26](PGCOMPARE-2026-07-26.md))
left one number unexplained: kevy answers a random point read in 71 µs —
2.8× faster than PG — and an indexed lookup in 393 µs, 2.2× *slower*,
while holding the entire dataset in RAM against PG reading from a 2 GB
cache over 17 GB of disk. Same engine, same data, same run. That ~320 µs
is not I/O, not memory, not hardware.

This decomposes it. **74 % of it is the shard fan-out, and with the
fan-out removed kevy's index path is 2–3× faster than PostgreSQL's.**

## The mechanism

`crates/kevy-rt/src/exec_build.rs`:

```rust
Route::Extension => {
    let targets = (0..self.nshards)          // every shard
        .map(|s| (s, Op::Extension { argv: argv.clone() }))
```

`IDX.QUERY` is an extension op, so it goes to **every shard** and the
origin merges the chunks. PostgreSQL does one btree descent in one
process.

The cause is a dimension mismatch, not an inefficiency: **the index is
partitioned by row key, and the query arrives by indexed value.** Any
shard could hold a match, so all of them must be asked. A point read
hashes to exactly one shard and pays one hop — which is why the same
engine wins that shape and loses this one.

## The measurement

Vary the shard count and change nothing else. `pk` is the control: it
routes to one shard whatever the count, so whatever it does *not* do is
the fan-out. 500k rows × ~440 B, 2000 samples, no AOF, lx64.

| shards | pk p50 | pk p99 | **idx p50** | idx p99 | **page p50** | page p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 15 µs | 131 µs | **36 µs** | 53 µs | **43 µs** | 60 µs |
| 4 | 21 µs | 121 µs | 48 µs | 100 µs | 70 µs | 122 µs |
| 8 | 22 µs | 149 µs | 71 µs | 322 µs | 106 µs | 147 µs |
| 16 | 22 µs | 266 µs | **140 µs** | 382 µs | **157 µs** | 259 µs |

(The `shards=2` row was lost to a readiness race in the runner; the
trend across the other four is unambiguous, so it was not re-run.)

**The control holds** — `pk` p50 moves 15 → 22 µs across a 16× change in
shard count, which is the routing constant, not fan-out.

**The index shapes scale with the shard count** — `idx` p50 goes
36 → 140 µs (**3.9×**) and `page` p50 43 → 157 µs (**3.7×**). At 16
shards roughly **104 of the 140 µs — 74 % — is fan-out**: sixteen
messages, sixteen `argv` clones, sixteen replies, one merge, to return
twenty rows.

## What that means for the model

At one shard, `IDX.QUERY` answers in **36 µs p50 / 53 µs p99**. PostgreSQL
answered the same shape in 126 µs (round one) and 176 µs (round three).

> **kevy's index engine is already 2–3× faster than PostgreSQL's btree
> path. Every bit of the loss is the scatter-gather around it.**

That reframes the problem. It is not "can a KV engine's index ever match
a relational planner" — it already does, comfortably. It is: **how does
an index query avoid asking shards that cannot hold a match.**

## The tension any fix must resolve

Partitioning the index by *indexed value* turns 16 hops into 1. The bill
arrives on the other side: writing a row would then have to update index
entries on a **different** shard, turning a local write into a
cross-shard one — and the write path is where kevy currently wins 20×.

> Can index queries be made to ask only the shard that can answer,
> without giving up the local write?

Directions, none of them started, all needing a design round:

1. **Value-partitioned index with an asynchronous write side** — trades
   index visibility immediacy for query locality.
2. **Dual index** — a local copy for write speed plus a value-partitioned
   copy for read speed; trades memory and consistency maintenance.
3. **Query pruning** — the origin decides which shards *can* match before
   asking (per-shard value-range summaries, or a Bloom filter per shard
   per index). Keeps writes local; costs a summary structure and its
   maintenance.
4. **Fewer, denser serving shards** — `--accept-shards` (v1.30) already
   proved that fewer, denser shards can win by +10.6 % on a different
   axis. The 1-shard row above is that idea taken to its limit for
   index-heavy workloads.

## Caveat this measurement does not cover

These are **single-connection latencies**. One shard answering in 36 µs
does not mean one shard is the right deployment: sixteen shards exist to
serve sixteen cores concurrently, and collapsing them trades throughput
for latency. A serious fix has to hold both — which is exactly why
pruning (3) and dual-index (2) are on the list next to the shard-count
lever (4), rather than "just run one shard".
