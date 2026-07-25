# Where IDX.QUERY's time goes: 74 % of it is the shard fan-out

The PostgreSQL comparison ([PGCOMPARE-2026-07-26](PGCOMPARE-2026-07-26.md))
left one number unexplained: kevy answers a random point read in 71 µs —
2.8× faster than PG — and an indexed lookup in 393 µs, 2.2× *slower*,
while holding the entire dataset in RAM against PG reading from a 2 GB
cache over 17 GB of disk. Same engine, same data, same run. That ~320 µs
is not I/O, not memory, not hardware.

This decomposes it. **74 % of it is the shard fan-out.** With the
fan-out out of the way the same index path answers in 36 µs — which
happens to be well under what PostgreSQL managed, though the point of
the number is what it says about our own structure, not the ranking.

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

At one shard, `IDX.QUERY` answers in **36 µs p50 / 53 µs p99** — for
reference, PostgreSQL answered the same shape in 126 µs and 176 µs, so
the index machinery is not what is expensive here.

> **The index engine is not the limit. The scatter-gather around it is.**

So the index machinery itself is not the limit; the routing around it
is. That is worth knowing before any design work, because it says the
work belongs at the level of *how an index is partitioned and reached*,
not at the level of scan algorithms, compression, or paging — none of
which is the main term in this 320 µs.

## What this says about our own model

Read this as a fact about kevy, not as a gap to a competitor's number.
The comparison is a health check taken afterwards; it is not the design
input, and designing to close a specific external figure is how a system
acquires special cases and stops at whichever local optimum happens to
match someone else's.

The model-native question the measurement raises is:

> In a share-nothing, thread-per-core engine where **everything else is
> routed by key**, what should an index *be*?

The fan-out is not a defect bolted onto the design — it is the honest
consequence of an index that spans a keyspace which is partitioned by
row key while queries arrive by indexed value. Two things follow, and
both are about internal coherence rather than about PostgreSQL:

- **The index is currently the one thing in the engine that is not
  routed like data.** Every other operation hashes to its owner and pays
  one hop. If index entries were themselves keys, an indexed lookup
  would route by the same rule as everything else — that removes a
  special case rather than adding one.
- **The write side already has the machinery** that such a shape would
  need. Cross-shard dispatch, escrow, the outbox and the CDC feed all
  exist, and the write path already fans out through index hooks. Whether
  index maintenance across shards can keep the derived-by-construction
  guarantee is a design question with an existing toolbox, not a new
  subsystem.

A third question the measurement does not answer but the model raises on
its own: **the index layer is 100 % RAM-resident by decision, not by
physics.** Tiering gave rows a hot/cold window; whether an access path
deserves one too is worth asking for the small-and-medium systems this
engine is aimed at, independently of any comparison.

None of this is started. It wants a design round that begins from the
model, reaches a shape that has no remaining room at the design,
principle, algorithm and model levels — and only then gets measured
against anything external.

## Caveat this measurement does not cover

These are **single-connection latencies**. One shard answering in 36 µs
does not mean one shard is the right deployment: sixteen shards exist to
serve sixteen cores concurrently, and collapsing them trades throughput
for latency. Any design that comes out of this has to hold both, which is why "run
one shard" is a measurement device here and not a conclusion.
