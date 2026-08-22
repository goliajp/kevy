# FINDING 2026-08-22 — at matched durability kevy has spent its flush budget; PostgreSQL has 38% of its left

**Status**: CLOSED. Measured three times, ~1% spread, and the throughput
identity closes on both engines.

## The question

At `appendfsync = always` and 64 clients, kevy serves 7,316 writes/s
against PostgreSQL's 28,877. Two readings fit that equally well and lead to
opposite designs:

- batching is not happening — each write pays its own flush, and a commit
  window is the answer;
- batching is happening and the device is the floor — and then a window
  buys nothing, because the flushes are already as few as they can be.

Choosing between them by argument is the hand-wave the perf methodology
bans, so this counts them.

## Method

`bench/fsyncprobe.sh`, on lx64, over a window containing nothing but the
write shape (`bench/pgconc.py --shapes write`). Counted system-wide with
perf, because PostgreSQL forks a backend per connection and per-pid
attachment misses every backend the sweep creates.

Both durability interfaces are counted, which is the part that took three
tries to get right: `syscalls:sys_enter_fdatasync` /
`sys_enter_fsync`, **and** `io_uring:io_uring_submit_req` filtered to
opcode 3. kevy runs the io_uring reactor here, so its barrier is
`IORING_OP_FSYNC` and never becomes a syscall at all.

Two independent witnesses, both required by the method rather than
decorative:

- **the device's own ceiling** — a bare `fdatasync` loop on the same
  filesystem: **1,212/s (825 µs each)**;
- **the box's idle noise** — 2.5 fsync/s from everything else running on
  the machine, subtracted from both engines.

## Result — three runs

| | writes/s | flush/s | writes per flush |
|---|---:|---:|---:|
| kevy `always` | 7,316 / 7,407 / 7,407 | 1,549 / 1,555 / 1,562 | **4.72 / 4.76 / 4.74** |
| PostgreSQL 18 | 28,877 / 27,739 / 29,775 | 749 / 749 / 790 | **38.53 / 37.04 / 37.69** |
| device, bare | — | **1,212** | — |

The identity `writes/s = flush/s × writes-per-flush` closes on both:
1,549 × 4.72 = 7,311 against a measured 7,316; 749 × 38.53 = 28,859
against 28,877.

And the two ratios multiply out to the observed throughput gap: batch size
38.53 / 4.72 = **8.16**, flush rate 1,549 / 749 = **2.07**, and
8.16 ÷ 2.07 = **3.94** against a measured 28,877 / 7,316 = **3.95**.

## What it says

**Neither of the two readings was right.** Batching is happening — 4.72
writes ride each flush, not one. But it is 8× weaker than PostgreSQL's,
and kevy pays for the shortfall in flushes:

- **kevy is at the device's flush ceiling.** 1,549/s against a bare-loop
  1,212/s — it exceeds the single-file figure because eight shards hold
  eight independent AOF files, so eight flush streams overlap. There is no
  headroom left to buy.
- **PostgreSQL is at 62% of that ceiling.** 749/s. It has room it is not
  using, because it does not need it: 38.5 writes ride each flush of its
  single WAL.

So the constraint is not the device, and it is not an absence of batching.
**It is that kevy needs eight times as many durability barriers for the
same work**, and the per-shard split is the multiplier — eight logs
competing for one device against PostgreSQL's one.

## Consequence for the design

This settles the open question in
`.claude/rfcs/2026-08-22-rds-side-representation-and-paths.md` §5, which
deliberately did not pre-judge it. The recoverable term is **batch size**,
it is measured at 8× away, and the flush budget it would spend is already
exhausted — so the gain is not "issue fewer flushes" but "carry more per
flush", which the current mechanism cannot do by construction:

- the group-commit bracket's unit is one socket read of one connection
  (`kevy-rt/src/inbox.rs:86-89`), so independent connections cannot share
  one;
- in lane mode the sharing that does occur is emergent — records that
  happen to arrive while one flush is in flight ride the next
  (`kevy-rt/src/aof_writer.rs:252-277`) — and the resulting 4.72 is
  whatever that accident yields, not a chosen window;
- durability is per-shard by construction, and no object in the structure
  can hold a batch that spans shards.

## What this does not say

It does not say a time-window commit would reach 38. PostgreSQL's 38.5
rides on one WAL; a kevy window would still be per-shard, so the ceiling
for an eight-shard server is eight windows' worth. What the measurement
establishes is the size of the term and that it is not the device — not
what a redesign would achieve.

## Note on the probe, because it failed three times looking like data

1. `perf` cannot exec a shell function, so the workload never ran and the
   counters came back empty — which formatted into the report as
   `fdatasync= fsync= over s`, a sentence with holes that reads as a result.
2. Per-pid attachment missed PostgreSQL's 64 forked backends and would have
   reported PostgreSQL as barely fsyncing at all.
3. Counting syscalls alone was blind to `IORING_OP_FSYNC` and produced
   "8,929 writes per fsync" — a number that looks like spectacular batching.

The third was caught only because the device ceiling was measured beside
it: 1,100 flushes/s makes that number impossible rather than merely
surprising. That is what the methodology means by hanging an unrelated
witness on every measurement. The probe now refuses an empty count and
refuses a store on tmpfs — the latter because the run that verified the
io_uring filter put its data directory under `mktemp -d` and measured
34,500 fsync/s on a device that does 1,200.
