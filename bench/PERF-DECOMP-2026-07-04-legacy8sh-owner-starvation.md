# PERF-DECOMP 2026-07-04 — legacy_8sh_set decay: owner-shard starvation, not per-op cost

**Campaign**: legacy_8sh 渐进衰退(task #10;evidence base
`PERF-FINDING-2026-07-03-legacy8sh-set-bimodal.md`)。Phase A first
pass on lx64: matched-build profiling of 4ecd017 ("OLD", the
baseline-era code, reproduces 9.98M today) vs v2.0.20 ("NEW", 8.5–9.1M
today). Both plain `release` + `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`
+ `STRIP=false` env (identical codegen to shipped; symbols retained —
NB the workspace `[profile.release] strip = true` had made first-pass
perf data address-only).

## Workload shape (matters!)

`legacy_8sh_set` = plain-mode `redis-benchmark -t set -c 50 -P 256`
with the **fixed default key** → a single key owned by ONE shard.
~7/8 of requests arrive on non-owner (origin) shards and take the
cross-shard inbox path. This angle is a **cross-shard routing + owner
saturation benchmark**, not a keyspace benchmark.

## Evidence

### 1. Per-op cost is IDENTICAL — the decay is utilization

8-second `perf stat` windows mid-steady-state:

| | OLD (10.06M rps) | NEW (8.62M rps) |
|---|---|---|
| unhalted cycles | 133.6B | 112.5B (**-16%**) |
| instructions | 253.6B | 207.1B |
| IPC | 1.90 | 1.84 |
| **cycles / op** | **1660** | **1631 (±2% = equal)** |
| context-switches | 308k | 131k (-58%) |
| dTLB-load-misses | 7.3M | **19.4M (×2.6)** |
| branch-misses | 49.5M | 67.6M (+37%) |

NEW executes the same work per op but **runs 16% fewer total cycles**
— cores are halted more. A busy-poll server that halts is a server
whose pipeline has bubbles.

### 2. Per-thread cycles: the owner halts more

| thread | OLD (8s) | NEW (8s) |
|---|---|---|
| owner shard | **37.6B** | **33.6B (-12%)** |
| 7 origin shards | 10.6–19.6B (avg 14.9B) | 3.3–18.1B (avg 11.4B) |

Throughput ratio 10.14/8.93 = 1.136 ≈ owner-cycles ratio 37.6/33.6 =
1.12: **throughput tracks owner executed cycles almost exactly**. The
owner is the bottleneck in both eras; NEW's owner sits halted ~12%
more (inbox runs dry between forwarded batches). Origins at 3.3B are
reply-starved, not lazy.

### 3. spin_limit hypothesis REFUTED

NEW at `[advanced] spin_limit` 256 / 4096 / 65536 → 8.25M / 7.97M /
8.27M. Spinning longer does not close the gap ⇒ not premature
parking; the refill latency is upstream (origin batching / wake /
reply-ring dynamics), or in the owner's own drain cadence.

### 4. Mode-attractor note

Long-N runs (300M ops, ~35s) compress the 06-03 short-N bimodality
into an 8.5–9.1M band on NEW; OLD stays 9.6–10.2M and was tight at
9.97–9.99M on short-N. Bimodality remains attributable to per-instance
draw (REUSEPORT accept distribution of the 50 conns over shards =
the owner's direct-conn share) — untested, next leg.

### 5. Suspect surface

`git log 4ecd017..v2.0.20 -- kevy-rt/src/{inbox,uring_inbox}.rs kevy-ring/` =
10 commits, all from the v1.23–v1.25 perf sprint (E8 acquire-load
fast path, E10 inline flush_wakes/drain_inbound, E15 fast-path
inline + cold-body outline, D1+D2 reactor bitmap fast-paths, A.9/K4
ready-set + file split, H1/H2 pub/sub chain, Axis E arm_conns walk).
That sprint measured and won on the **pinned** angles (+19% over
baseline, confirmed still true today); the single-owner cross-shard
shape was not in its perfgate set. `drain_inbound_core_slow` at 4.71%
self in NEW's profile (symbol absent in OLD) is the first concrete
suspect: identify what routes to the slow path on this shape.

## Next probes (in order, ~1 lx64-hour)

1. `drain_inbound_core_slow`: read current code; why slow-path on this
   shape; counter-patch a diag build if unclear.
2. Candidate-commit probes: build 0f9e1d7 (E15) and ce28b92 (D1+D2)
   parents/children, 1-angle × 2-instance probe each — the gradual
   decay likely decomposes into 2–3 steps of a few % (matches bisect
   history: 9.97 → 9.2 → 8.5/8.0 bands).
3. dTLB ×2.6: check pbuf slab / ring buffer allocation layout changes
   (kevy-madvise hugepage wiring era) — may be a second independent
   contributor.
4. Mode attractor: instrument owner identity + per-shard direct-conn
   count at accept; correlate with instance rps on short-N.

## Fix criteria (per campaign charter)

Whatever restores legacy_8sh throughput must NOT give back the pinned
+19% (both angle families go into the same perfgate run; legacy angles
get n>3 instances after the fix). Baseline re-record only after this
campaign closes — with both improvements and the restored legacy line.

## Assets (kept on lx64 for the next leg)

- worktrees `/tmp/kevy-old-wt` (4ecd017) + `/tmp/kevy-new-wt` (v2.0.20),
  both built unstripped with line tables.
- `/tmp/prof/*.{report,stat,data,perthread}` — 6 labeled instances + 2
  per-thread runs.
- scripts `/tmp/kevy-profile-one.sh`, `/tmp/kevy-perthread.sh`,
  `/tmp/kevy-spin-probe.sh`(pkill 模式已修), `/tmp/kevy-bisect-probe.sh`.
