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

## Round 2 (same day) — era sweep + step-1 attribution

### Era sweep (2 instances each, plain release, spin probe N=60M)

| ref | instances | read |
|---|---|---|
| v1.16.0 | 9.98M · 9.97M | full speed, tight — pre-decay |
| v1.17.0 | 9.59M · 9.59M | **step 1: -4%** |
| v1.18.0 | 9.21M · 8.87M | drift + spread begins |
| v1.22.0 | 9.59M · 9.21M | mixed |
| v1.23.0 | 8.02M · 8.26M | **step 2: the v1.23 perf sprint itself** |
| v1.24.0 | 9.20M · 8.26M | mode mixture |
| v1.25.0 | 8.56M · 8.53M | settled at today's band |

E15 (0f9e1d7) A/B: 8.56M vs parent 8.25M — E15 exonerated (both
already decayed; the v1.25-era micro-attacks are not the step).

### Step 1 = 286c4a2 (v1.17 INFO cross-shard observability)

The only hot-path commit in v1.16.0..v1.17.0. A/B/A:

- parent: 10.41M · 9.97M (0/2 low; old-era baseline never draws low)
- at 286c4a2 (5 instances): 9.58 · 9.99 · 9.98 · 9.58 · 10.41 —
  **2/5 draw a new 9.58M mode** that the parent never draws.

So the commit does not shift the mean uniformly — it **introduces a
lower attractor** and per-instance luck picks it. This is also the
2026-06-14 debt coming due verbatim: the v1.17 ship note said
"per-command TLS counters ~1-2ns, e2e throughput NOT perfgated on
Linux — asked dogfood to re-measure"; nobody ever did.

Micro-mechanism inside 286c4a2 still open (candidates: per-command
thread-local Cell access cost incl. lazy-init branch, the O(1)
`Store::expires` counter on every write's insert/remove path,
`ShardStats` Arc allocation adjacency). `ShardStats` is exactly 64 B
(8×AtomicU64) but only tick-published at 10 Hz — false sharing of the
slots themselves cannot cost 4%.

### Attribution so far

- **Step 1 (-4%, mode instability introduced): 286c4a2** — empirical,
  5+2 instances.
- **Step 2 (additional ~-10%): inside v1.22.0..v1.23.0** (the 16-attack
  perf sprint that bought pinned +19%). Needs its own within-sprint
  sweep — predicate: any instance < 8.8M (8.0–8.3 vs 9.2–9.6
  separation).

### Next probes (updated)

1. Step-2 sweep across the v1.23 sprint attack commits (same
   spin-probe harness, 2 instances/point; ~16 points ≈ 40 min).
2. 286c4a2 micro-mechanism: diag builds of the commit minus (a) the
   start_command counter hook, (b) the expires counter — one angle
   probe each.
3. Fix design once both steps are pinned; joint perfgate (legacy n>3
   + pinned angles must hold +19%).

## Round 3 (same day) — step-2 pinned to 4fa4631; two fix designs falsified

### Sweep 2 (v1.23 sprint hot commits, 2 instances each)

| ref | instances |
|---|---|
| **4fa4631^ (anchor)** | **9,973,405 · 9,990,138 — full speed** |
| 4fa4631 (nap-rung removal) | 7,247,252 · 7,973,422 |
| ce28b92 D1+D2 | 8,248,556 · 8,544,574 |
| b71f788 D3 | 8,249,691 · 7,983,004 |
| 341791d D5-infra | 7,715,058 · 7,732,956 |
| acca152 E8 | 7,715,058 · 7,970,244 |
| 36d06f1 E9 | 7,975,542 · 7,976,602 |
| 17ccdbc E10 | 7,979,784 · 7,707,129 |

**Step 2 = 4fa4631, single-commit -20~27%** — exactly the commit's own
foreseen "−18~21 % 8-shard throughput … revisited as a v1.22.x
follow-up if a workload re-surfaces it" (the follow-up never
happened). Later sprint commits recover partially (~8.5M band).

### Phase B round 1 — falsified designs (with telemetry)

1. **Batch-gated nap** (nap 200 µs once per idle episode iff last
   inbound drain ≥ 4 msgs; -c1-safe by construction): implemented on
   `feature/perf-legacy8sh-nap-rung`, correctness green (kevy-rt 38 +
   blocking_cross_shard 8/8), **no throughput effect** — [8.86 · 8.26
   · 8.55]M, same band as unfixed. Diag telemetry shows the rung DOES
   fire (~50% of ladder exits nap, avg observed batch 650–1224 on
   origin shards) — the mechanism fires, the benefit doesn't come.
2. **Exact-old-ladder replica** (unconditional nap before park):
   **also no effect** — 8.88M.

**Conclusion: the old ladder's benefit is not reproducible by
re-adding the nap to v2.x code.** The -20% loss at 4fa4631 and the
inverse gain do not commute across the 20 releases in between — the
nap's aggregation benefit depended on the OLD era's cross-shard wake
topology, which the E-series (E8 acquire fast path, E10 inlining,
E15 outline) and D-series (bitmap open-loop paths) have since
restructured.

### Next thread (the real Phase B aperture)

Diff the cross-shard **wake topology** old vs new end-to-end:
`send_to` (similar in both: deferred `pending_wakes` / dirty-bit),
**`flush_wakes`** (where waker-pipe writes actually happen — cost,
批量, parked-gating), `parked[]` semantics, and who wakes whom how
often per second on this shape (add wake-count diag both eras).
Quantify: waker-pipe writes/s, wakes consumed/s, avg drain batch at
owner + origins, old vs new. The owner's 12% halt deficit must map to
a concrete wait-for-signal gap.

Branch `feature/perf-legacy8sh-nap-rung` holds the falsified
experiments + diag (NOT for merge as-is; telemetry eprintln must be
removed or feature-gated before any land).
