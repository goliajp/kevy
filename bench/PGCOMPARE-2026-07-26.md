# kevy 4 vs PostgreSQL 18 — measured, including where kevy loses

> **Superseded, and two of its read numbers were not comparing like with
> like.** The harness that produced this run timed kevy read shapes that
> asked for **no columns**, against SQL that selected two or three, and the
> kevy table declared six of the seven columns its rows carried. Both were
> fixed in 5.4. Re-measured fairly, the secondary-index lookup published
> here as 212 µs against PostgreSQL's 126 is a tie within 4% (160 against
> 154), and the list page's 1.9× gap is 1.2×.
>
> The write and memory rows do not have that defect, and the
> matched-durability figure here (3,097 µs) is the one 5.4 returns to after
> fixing a regression that made it 47 ms in 5.2 and 5.3.
>
> Current numbers: [`docs/rds-workloads.md`](../docs/rds-workloads.md).
> This file stays as the record of what was run and when — not as a source
> of current figures.

The repo had never benchmarked kevy against a relational database. It
has valkey numbers and embedded-KV numbers; every "faster than an RDS"
sentence was inference. This is the measurement, in two rounds — a
dataset that fits in cache and one that does not — and it contradicts
the inference more often than it confirms it.

The short version, after three rounds: **kevy wins writes, random-key
reads, bulk load and disk footprint; PostgreSQL wins indexed lookups,
list pages, memory, and writes at matched durability.** Which of those
matters depends entirely on the workload, so every column is below
rather than the flattering subset — including the round that was run
specifically to check whether a PG win was an artifact, and confirmed it
was not.

**Reproduce:** `bash bench/pgcompare.sh 2000000 400` for round one,
`3000000 4000` against a memory-capped PG for round two (harness:
`bench/pgcompare.py`). Run 2026-07-26 on lx64 — 16 cores, NVMe, the
bench account, nothing else of ours running.

## What was compared, and how it was kept fair

**One harness drives both.** A single Python process runs the same
timing code against PostgreSQL (psycopg 3.3) and kevy (raw RESP). Using
each system's native tool (pgbench vs redis-benchmark) would have
compared the tools. Client cost is additive and lands on both sides, so
it *compresses* ratios: every gap below is a lower bound.

**PostgreSQL 18.4 runs stock**, as asked — `shared_buffers=128MB`,
`fsync=on`, `synchronous_commit=on`, `full_page_writes=on`. That is
conservative for PG; a tuned instance would improve on its read numbers
here. It ran in its own container on its own port, never the ones this
box already hosts.

**kevy is measured in four modes, because its stock config has AOF
off.** Comparing "no durability" to "fsync per commit" would be a
category error, so every durability level is reported and the reader
picks the row matching what they would run:

| mode | durability |
|---|---|
| `none` | AOF off — kevy's literal default, the in-memory ceiling |
| `everysec` | AOF on, background fsync each second (loses ≤1s) |
| `always` | AOF on, fsync per write — the closest match to PG's stock |
| `tiered` | `everysec` + a 2 GB RAM budget, rows spill to disk |

The fsync policy is asserted at runtime (`CONFIG GET appendfsync`) and
the run refuses to start if it disagrees — the first attempt at this
benchmark silently ran `always` as `everysec` because `--appendfsync`
is not a CLI flag, and produced a mislabelled row.

**The workload** is one table, the single-table shape kevy claims:
2,000,000 rows × ~440 B = **843 MB of CSV**, `id / name / dept / age /
ts / pad`, with equivalent access paths on both sides — PK, an index on
`age`, and a composite `(dept, age)`. 5,000 samples per query shape.

## Results

| engine / mode | load MB/s | pk p99 | idx p99 | page p99 | write p99 | disk KB per CSV-MB | RSS KB per CSV-MB |
|---|---:|---:|---:|---:|---:|---:|---:|
| **postgres 18 stock** | **84.7** | 85 µs | **126 µs** | **131 µs** | 1689 µs | **1204** | **625** |
| kevy `none` | 64.1 | 247 µs¹ | 191 µs | 238 µs | **63 µs** | 0 | 5780 |
| kevy `everysec` | 58.8 | **74 µs** | 212 µs | 248 µs | **62 µs** | 1322 | 6153 |
| kevy `always` | 3.4 | 77 µs | 184 µs | 241 µs | 3097 µs | 1323 | 6167 |
| kevy `tiered` | 59.5 | 80 µs | 224 µs | 238 µs | 161 µs | 1684 | 5463 |

¹ the odd one out; the same operation is 74–80 µs in every other kevy
mode, so read it as first-touch noise, not a property.

`pk` = point lookup by primary key · `idx` = lookup on the secondary
index, LIMIT 20 · `page` = `WHERE dept = ? AND age BETWEEN ? AND ?
ORDER BY age LIMIT 20` · `write` = single-row update.

## Reading it honestly

**Reads: PostgreSQL wins the index-driven shapes.** 126 µs vs 184–224 µs
on the secondary-index lookup (~1.5×) and 131 µs vs 238–248 µs on the
list page (~1.8×). Point lookup by PK is a tie (85 µs vs 74–80 µs). At
this scale the whole dataset is in page cache and PG's btree plus its
planner are simply very good. **The claim that kevy reads an order of
magnitude faster than an RDS is false at this size and shape.**

**Writes: it depends entirely on the durability you accept.**
- At `everysec` — lose at most one second — kevy is **27× faster**
  (62 µs vs 1689 µs).
- At matched durability (`always`, fsync per write) **PostgreSQL is
  1.8× faster** (1689 µs vs 3097 µs). PG batches WAL and group-commits;
  kevy fsyncs per command. This is the single most important row in the
  table and it is not in kevy's favour.

**Bulk load: PG wins.** `COPY` at 84.7 MB/s against 58.8–64.1 MB/s of
pipelined RESP. With `always`, kevy's load collapses to 3.4 MB/s (25×
slower) — per-command fsync during ingest.

**Disk: PG is the most compact.** 1204 KB per CSV-MB against kevy's 1322
(`everysec`). The tiered run pays 1684 because the value log holds
spilled rows on top of the AOF. Everything is 1.2–1.7× the raw CSV.

**Memory: PG wins by roughly 10×, and this is structural.** 625 KB per
CSV-MB against 5780–6167. PG keeps 128 MB of shared buffers and leans on
the OS page cache; kevy holds rows in its own heap. Tiering is the
answer to that and it works *on its own terms* — with a 2 GB budget,
`used_memory` settled at **1.96 GB, inside the budget**, 424 k keys and
474 MB of values spilled to the value log, 427 MB held as the
RAM-resident index layer (indexes hot, rows cold, by design). But the
**process RSS was 4.39 GB — 2.24× the logical bound** — glibc heap
fragmentation under demote/promote churn of ~400 B values, the same
allocator behaviour documented in
[`PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md`](PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md)
and reclaim-proof there (`malloc_trim` and `MALLOC_ARENA_MAX` both
measured as no-ops). So tiering bounds what kevy *accounts*, and on this
workload the resident set is a little over twice that. An operator sizing
a container must use the RSS number, not the budget.

## What this says about "kevy can replace a basic RDS"

For **single-table, declared-access-path workloads it is a real
alternative**, and the reason is writes, not reads: at one-second
durability it absorbs updates 27× faster, which is exactly the shape
that makes teams put a cache in front of their RDS in the first place.

It is **not** faster at reading — PG stock beats it on both index-driven
query shapes — and it costs about ten times the memory for the same
data. Neither of those is a bug to be fixed later; they follow from
holding rows in-process, and from an RDS's planner being good at the
work it was built for.

## Round two — the dataset that does not fit

The first round's obvious objection is that 843 MB sits in page cache on
a 62 GB box, so PG never touched a disk. Round two removes that: **3M
rows × 4 KB of incompressible payload = 11.5 GB of CSV, 17 GB on PG's
disk**, with PostgreSQL held to a **2 GB memory cgroup** (page cache
included, so misses are real) and kevy given a **1 GB tiering budget**.
Each engine stays near 2–3 GB resident by its own mechanism, against
data six times that.

| engine / mode | load MB/s | pk p99 | idx p99 | page p99 | write p99 | disk KB/CSV-MB | RSS KB/CSV-MB |
|---|---:|---:|---:|---:|---:|---:|---:|
| postgres 18, 2 GB cap | 58.3 | 184 µs | **131 µs** | **118 µs** | 1684 µs | 1434 | **52** |
| kevy `everysec`, no budget | **359.5** | **58 µs** | 321 µs | 429 µs | **79 µs** | **1060** | 1516 |
| kevy `tiered`, 1 GB budget | 288.5 | 78 µs | 266 µs | 360 µs | 258 µs | 2087 | 267 |

**The prediction was half wrong, and the half that failed is instructive.**
Going in, the expectation was that reads would flip once the data stopped
fitting. Only the *random-access* shape flipped:

- **`pk` — random primary key across 3M rows.** PG 184 µs, kevy 58 µs
  untiered and **78 µs tiered** — 2.4–3.2× — and the tiered number is the
  interesting one, because those rows are *on disk*: a cold read costs
  one pread and still beats PG's buffer-pool miss.
- **`idx` and `page` — PG still wins**, 131 µs vs 266–321 µs and 118 µs
  vs 360–429 µs. The obvious suspicion was that this was an artifact:
  `age` takes 60 values and `dept` takes 8, so `LIMIT 20` returns the
  same handful of rows every time and their heap pages never leave PG's
  cache however large the table is. Round three tested that suspicion
  and **it was wrong** — see below.

Tiering, on the other hand, did exactly what it exists for, and the
gauges are in the row rather than asserted:

| | |
|---|---|
| rows demoted to disk | **2,946,397 / 3,000,000 = 98.3 %** |
| `used_memory` | **541 MB**, inside the 1 GB budget |
| index layer resident | 613 MB (indexes hot, rows cold — by design) |
| value log on disk | 11.3 GB |
| **RSS** | **2.95 GB vs 17.95 GB untiered — 6.1× less** |

So the same 12 GB of data needs 18 GB of RAM without tiering and 3 GB
with it, at a read cost of 78 µs instead of 58 µs on the random shape.
PG still holds the memory crown (614 MB), but the gap closes from 29×
to 5×.

Two more inversions from round one: **kevy now loads 6× faster** (359 vs
58 MB/s — PG's COPY pays TOAST and WAL on 4 KB rows while kevy's cost
per MB drops as values grow), and **kevy is now more compact on disk**
(1060 vs 1434 KB per CSV-MB — no per-row MVCC or page overhead). The
tiered row pays 2087 because the value log and the AOF both hold the
data.

## Round three — the same test with predicates that actually scatter

Round two hedged: PG's win on the indexed shapes might have been the
low-cardinality predicates keeping a tiny working set hot. The hedge
favoured kevy, so it had to be tested rather than left standing.

The workload gained a `sku` column — one value per ~20 rows, placed at
random — and the list page became a **random time window anywhere in the
table** instead of a fixed low-cardinality slice. Both engines index it
the same way. Everything else is round two: 11.6 GB of incompressible
CSV, 17 GB on PG's disk, PG capped at 2 GB, kevy at a 1 GB budget.

| engine / mode | load MB/s | pk p99 | idx p99 | page p99 | write p99 | disk KB/CSV-MB | RSS KB/CSV-MB |
|---|---:|---:|---:|---:|---:|---:|---:|
| postgres 18, 2 GB cap | 56.4 | 201 µs | **176 µs** | **127 µs** | 1731 µs | 1440 | **60** |
| kevy `everysec`, no budget | **363.9** | **71 µs** | 393 µs | 237 µs | **87 µs** | **1063** | 1540 |
| kevy `tiered`, 1 GB budget | 290.7 | **71 µs** | 385 µs | 278 µs | 223 µs | 2093 | 285 |

**The hedge does not survive.** With predicates that genuinely scatter
across 3M rows and a cache holding an eighth of the data, PostgreSQL
wins the indexed lookup by **2.2×** (176 µs vs 385–393 µs) and the list
page by **1.9–2.2×** (127 µs vs 237–278 µs) — a wider margin than when
the working set was hot, not a narrower one. kevy keeps the random-key
read at **2.8×** (71 µs vs 201 µs), unchanged.

And the interesting part is that this is **not an I/O story**: the
untiered kevy run holds all 12 GB in RAM and still answers the indexed
lookup in 393 µs against PG's 176 µs from a 2 GB cache over 17 GB of
disk. The cost is in the query path — index scan, assembling twenty
rows, encoding the reply — not in fetching the data. That is a concrete
optimisation target with a number attached, which is worth more than the
hedge was.

One thing the round does confirm: `tiered` and `everysec` post the same
indexed-lookup latency (385 vs 393 µs) although 98.3 % of rows are on
disk in the tiered run. The `VALUES` columns answer that query from the
index without touching a row, exactly as the index-only claim says.

### The compression difference, measured separately

The first attempt at round two used a constant pad (`"x" * 4000`).
Past PG's ~2 KB TOAST threshold that compresses about **25:1** — 12 GB
of CSV became **488 MB** on PG's disk, which then fit entirely inside
the 2 GB cap the run existed to overflow. Every column of that run was
void and it was discarded.

The number itself is real and worth stating on its own: **PostgreSQL
compresses large repetitive column values; kevy does not** (no value
compression, a consequence of the zero-dependency rule). For a table of
JSON or prose, PG's disk footprint can be a small fraction of kevy's.
The table above uses random hex precisely so that this difference does
not silently distort the other six columns.

## Boundaries of this measurement

- **Two sizes, three rounds.** 843 MB (fits in cache) and 11.6 GB
  (does not), the latter with both low- and high-cardinality
  predicates.
- **Round three closed the cardinality question**; rounds one and two
  used low-cardinality predicates and their `idx` / `page` columns
  should be read through round three's numbers.
- **Single connection, sequential.** These are latencies, not
  throughput under concurrency, where PG's per-connection backend model
  and kevy's per-core sharding diverge sharply.
- **Stock PG** (plus a memory cgroup in round two). Tuning
  `shared_buffers`, `synchronous_commit` or `commit_delay` moves PG's
  numbers, mostly upward.
- **No JOINs, no ad-hoc queries** — the shapes kevy refuses by design
  are absent, so this measures the slice where both can compete.
- **Single connection, sequential.** These are latencies, not
  throughput under concurrency, where PG's per-connection backend model
  and kevy's per-core sharding diverge sharply.
- **Stock PG.** Tuning `shared_buffers`, `synchronous_commit` or
  `commit_delay` moves PG's numbers, mostly upward.
- **No JOINs, no ad-hoc queries** — the shapes kevy refuses by design
  are absent, so this measures the slice where both can compete.
