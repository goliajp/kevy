# The "third seat" splits in two: a gauge artifact and the capped final swap

Release-train S5 prelude (2026-08-10), following S4
(`bench/FINDING-2026-08-09-aof-offload-s1-and-the-rewrite-seat.md`).
All cells: box NVMe (TMPDIR=captmp), `KEVY_AOF_OFFLOAD=1`, 4 threads,
tailgate workloads. **Single runs each — the variance table below is
itself a finding; nothing here is rankable run-to-run yet.**

## The data matrix

| run | rewrite | tick gate | mixed gap | mixed PING p999/max | firehose gap | firehose PING p999/max |
|---|---|---|---|---|---|---|
| S1 (pre-S4) | on | 256-iter | 818 ms | (green) | 9 514 ms | (green) |
| S4 | on | 256-iter | 581 ms | 9.8 ms / 345 ms | 314 ms | 61.7 ms / 292 ms |
| ablation | **off** | 256-iter | 949 ms | **1.0 ms / 12.7 ms** | 205 ms | 15.6 ms / 21.2 ms |
| gauge fix | on | 256-iter ∨ comps≥8 | 1 657 ms | 8.6 ms / **396 ms** | 459 ms | 71.3 ms / **541 ms** |

## What the matrix proves

**1. The gauge over-reports under saturation (rewrite-off row).** Mixed
reads a 949 ms "reactor gap" while its worst client PING in 60 s is
12.7 ms — a 949 ms genuine stall would have parked a probe PING for
949 ms. Both reactors gate the tick-clock check on a 256-iteration
counter (idle/park escapes aside); a saturated shard checks the clock
once per ~256 large iterations, and the gauge reports that accumulated
busy window as lateness. The gauge's own comment ("the tick's lateness
IS the single-iteration stall upper bound") is only true off-saturation.

**2. My batch-count fix did NOT close the artifact — prediction
refuted.** `comps.len() >= 8` as the "big iteration" proxy assumed
batch count ~ work size. Wrong under pipelining: one completion can
carry a P16 burst (16 commands × 1 KiB), so a mixed-cell iteration with
comps < 8 still does ms-scale work, and 256 of those still span ~1.6 s.
Batch COUNT is not work SIZE. (The fix is still right for what it
provably does: BLOCK/WAIT timeout slop under saturation shrinks
whenever batches ARE big; perfgate validation pending.)

**3. The rewrite-ON runs contain a REAL client-visible stall: the
capped final swap.** Rewrite-off, worst PING = 12.7 ms; rewrite-on,
worst PING = 345-541 ms across three runs, both cells. S4 hands tee
generations to the worker but caps handoffs at 4 (livelock guard) —
under sustained ingest the 5th generation is bounded-LARGE and the
reactor pays its append + `sync_all` + rename synchronously. The seat
S4 shrank from 9.5 s did not vanish; its floor is the cap policy.

**4. Run-to-run variance is disqualifying for single-run judgment.**
Mixed gap across four runs: 581 / 818 / 949 / 1 657 ms. Any future
attack on these cells needs median-of-N ≥ 3 with stdev reported, per
the methodology's single-run trigger word.

## Named attack surface for S5 (each needs its own slice)

- **A. Gauge semantics** (measurement, not perf): make the tick-gap
  gauge measure true per-iteration stall. Batch-count gating is
  refuted; candidates: clock every work-iteration (costs one vDSO read
  on the -c1 path — needs perfgate proof it's noise), or a bytes/ops
  processed proxy instead of completion count.
- **B. The capped swap** (the real seat): today the 4-handoff cap
  converts "livelock risk" into "one bounded-large synchronous append".
  Candidates: adaptive cap (hand off while the tee shrinks
  generation-over-generation, cut over only when it stops shrinking),
  rate-gating begin (don't start a rewrite while ingest > disk
  bandwidth), or chunked final swap (append the last tee in bounded
  slices between reactor iterations).
- **C. Bench infra**: tailgate needs an N≥3 median mode before any
  A/B on these cells is admissible.

## Status

- Tick-cadence fix (`22fc7e6b`, this branch): merge-gated on perfgate
  (hot-loop change), kept for its BLOCK-timeout honesty even though it
  does not clear the gauge artifact.
- The 100 ms reactor-gap bar stays RED and stays honest: part
  measurement artifact (A), part real capped-swap stall (B). Do not
  widen the bar; fix A so the number means what the bar says, then
  attack B.

## Postscript (same day): A, B, C landed — two seats fell, one named

Median-of-3 (C) first: the single-run spread was worse than feared —
baseline mixed gap median **3 077 ms** (484..5 939), one run showing a
**6.04 s client-visible PING max**; firehose PING p99.9 median 117 ms
(the earlier "double green PING" was single-run luck).

Three changes, each box-gated (crashgate/repligate ×2 modes, perfgate
on the hot-loop one, 239 release suites):

1. **A — work-iteration tick gate** (`TICK_CHECK_WORK_ITERS = 4`):
   batch-count refuted, work-iteration counting landed; perfgate PASS
   (every-4th-work-iteration vDSO read is noise).
2. **B — divergence defer** (`40bfc07b`): generations must halve to
   keep handing off; a non-shrinking generation defers the rewrite
   (abort + re-anchor the growth rule) instead of force-swapping.
   Mixed cell: gap median 3 077 → **93 ms**, max PING 6 s → **86 ms**.
3. **B2 — worker-side unlink** (`35771876`): B moved the firehose cost
   into its own defer path — deleting the multi-GB abandoned image on
   the reactor contends on the fs journal under a saturated disk
   (measured: gap median 576 → 1 485 ms, PING max 1.78 s once defers
   began firing). PersistJob::Remove ships the unlink to the worker.
   Firehose: PING p99.9 median 118 → **84.7 ms — first-ever green**,
   max 1.78 s → 141-388 ms.

Remaining after all three (median-of-3): mixed gap 308 ms (44..320,
median straddles the bar run-to-run), firehose gap 514 ms (476..516,
tight). One suspect covers every remaining observation, including
"gauge high while the probe shard's clients never wait" (the stalls
are on the OTHER shards): **the rewrite tee is a single Vec grown by
doubling on the reactor's append path** — at GB scale one realloc is a
0.2-0.4 s memcpy, paid inside command processing. Next slice (S5-D):
chunked tee (fixed-size chunk list, no realloc above chunk size;
hand-off and finish write the chunk list as-is). Arithmetic to verify
in Phase B: biggest realloc ≈ half the largest generation; firehose's
tight 476-516 ms band should collapse to chunk-scale.

## S5-D REFUTED and reverted — the tee realloc was not the seat

Chunked tee implemented (8 MiB chunks, io::Write drop-in, chunk-list
handoff — all local + box gates green) and median-of-3 measured
**worse**: firehose PING p99.9 median 84.7 → 181 ms (129..267,
non-overlapping with the tight 84.7..87.8 before — a real regression),
gap median 514 → 720 ms; mixed grew a 1.04 s client max. Branch
deleted, not merged.

Two lessons, both methodology triggers I walked into:

1. The "0.2-0.4 s GB memcpy" estimate was hand-waved from the cost
   table without a profile. Large-allocation realloc paths (mremap
   genre — and kevy runs its own kevy-alloc) do not necessarily copy
   at all; the estimate justified a slice that measurably regressed.
2. Two armchair candidates in a row (batch-count gauge proxy, tee
   realloc) both refuted on the box. The remaining 300-700 ms band
   does not get another guessed knife: the next slice (S5-E) is an
   INSTRUMENTED decomposition — perf record (on-CPU) + off-CPU/sched
   profiling of the reactor threads during the firehose cell, naming
   the seat by symbol before any code moves. Nothing merges on this
   surface until a profile names it.

Where the bar stands after revert (= merged develop, A+B+B2):
mixed gap median ~300 ms (straddling), firehose PING green at median
(84.7 ms) with gap median ~514 ms. Both cells' PING p99.9 medians are
within or at the bar; the reactor-gap bar is the open half.
