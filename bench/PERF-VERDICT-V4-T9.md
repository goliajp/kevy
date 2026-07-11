# T9 verdict — the earned position (v4, 2026-07-12)

Five lever candidates from the panoramic decomposition, each closed
by measurement — two by full implementation A/B, three refused at
their pre-implementation gates. This document is the "earn ceiling"
record the perf constitution requires before any position statement.

## Position (median-of-5 ± stdev, lx64, c50/c100 P16 -d 3)

kevy 3.18.0: GET 6.39M / SET 6.38M ops/s — 1.60x redis 8 (single
exec thread, 43% core use), 3.00x valkey 9.1 (33% of cycles in
pthread_mutex_lock across io-threads), 3.60x dragonfly (the
shared-nothing sibling; it uses BUNDLE + MSG_RING + direct-fd — all
three of our L5 candidates — and still spends 10,400 cycles/op,
2.2x ours).

## The five candidates

- L1 shared-read keyspace (seqlock read path) — REFUSED. Prototype:
  semantics sound (88M ops, zero torn reads; retry p99 = 0 under
  write pressure) but the per-op saving is 0.02-0.06µs against the
  0.3µs gate, because RequestBatch amortization already flattens the
  ring round-trip to a ~0.1µs ceiling. Prototype in-tree at
  crates/kevy-bench/examples/seqlock_probe/.
- L2 c50 under-saturation closure — REVERTED after full
  implementation: earlier blocking cost -16.6% to -39% across
  shapes; the identical redesign at spin-256 measured flat. The
  ladder's ~1,050 cycles/op at c50 is the shape of waiting.
- L3 per-conn CQE batching — REFUSED at gate: 33M recv CQEs, zero
  same-conn-same-reap repeats; the kernel's provided-buffer ring
  already coalesces P16 bursts to ~12.7 ops/CQE.
- L4 map hugepages/layout — REFUSED at gate: THP already backs
  98.4% of the tables (the allocator's 2MB-aligned path plus the
  box policy); the whole DRAM-stall pie is 5.7pp.
- L5 io_uring basket — REFUSED at gate: direct-fd surface
  0.30-0.46pp (ring-fd registration already took the bulk), BUNDLE
  merge surface literally zero on the main axis, MSG_RING wakes
  1.4-6.6/s at saturation.

## The c50 plateau's ownership

The 6.39M vs 8.0M mode gap lives in the load generator's in-flight
regime, not in recoverable server cost. Anchors: four shapes reach
the 8.0M plateau while client threads 6 -> 8 move nothing; the L2
full implementation proved a server idle policy cannot create the
difference; the 8.0M mode appears per-run, uncorrelated with
connection count, observed on c100 sessions pinned at 6.38M too.

## Per-op budget as measured (reconciled to -11%)

Kernel TCP send/recv ~44% (~1,900-2,075 cycles; enter already
amortized to ~27 ops/syscall at P16). Reactor body ~906 cycles (the
c50-specific ~1,050 proven to be waiting). Forwarding chain ~580
cycles (proven near-optimal by the L1 prototype).

## What we claim — and do not

We do not claim "cannot go faster." We claim: every candidate on
this table is closed with its reopen condition on record — L1: a
genuinely spread workload with high read ratio and saturated owners,
judged by whole-system A/B; L2: a real client shape beyond
redis-benchmark; L4: a slot-layout change that crosses cache-line
boundaries; L5: a connection-churn shape that inflates write-SQE
rates. Until a reopen condition fires, the next >=1.5x step is not
on this table; the correct restart is a fresh decomposition of the
new shape, never another polish pass over this one.
