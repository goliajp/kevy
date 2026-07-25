# kevy 4 vs PostgreSQL 18 — measured, including where kevy loses

The repo had never benchmarked kevy against a relational database. It
has valkey numbers and embedded-KV numbers; every "faster than an RDS"
sentence was inference. This is the measurement, and it refutes the
inference in two of five columns.

**Reproduce:** `bash bench/pgcompare.sh 2000000 400` (harness:
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

## Boundaries of this measurement

- **One size, one shape.** 843 MB fits comfortably in page cache on a
  62 GB box, which favours PG's read path. The comparison that would
  favour kevy — a dataset far larger than RAM, where PG goes to disk on
  every miss and kevy answers index-only queries from memory — is not
  measured here.
- **Single connection, sequential.** These are latencies, not
  throughput under concurrency, where PG's per-connection backend model
  and kevy's per-core sharding diverge sharply.
- **Stock PG.** Tuning `shared_buffers`, `synchronous_commit` or
  `commit_delay` moves PG's numbers, mostly upward.
- **No JOINs, no ad-hoc queries** — the shapes kevy refuses by design
  are absent, so this measures the slice where both can compete.
