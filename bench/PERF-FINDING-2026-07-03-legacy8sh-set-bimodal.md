# PERF-FINDING 2026-07-03 — legacy_8sh_set perfgate red: bimodal instances + 3-week-stale baseline, NOT a v2.1 regression

**Context**: v2.1 五轴收口 perfgate on lx64 (kernel 6.12, 13-day
uptime, default valkey container resident). Gate failed
`legacy_8sh_set` on the v2.1 branch → A/B/C bisect per methodology
(Pre-Phase-A gate: verify the gap is real before attacking).

## The A/B/C matrix (same box, same hour, N=30M ×8, 3 fresh instances per angle)

| Binary | legacy_8sh_set instances | median | vs floor 9,181,675 |
|---|---|---|---|
| v2.1 branch (`bce7be8`) | [9,199,671 · 8,576,952 · 8,556,796] | 8,576,952 | ✗ FAIL |
| master = v2.0.21 | [8,551,918 · 8,547,045 · 8,539,746] | 8,547,045 | ✗ FAIL |
| v2.0.20 (pre-hotfix) | [9,199,671 · 9,205,317 · 8,559,237] | 9,199,671 | ✓ PASS |

Every other angle: v2.1 ≡ master ≡ v2.0.20 within noise, and the four
pinned angles run **+10–19% above baseline** on all three binaries.

## Reading

1. **Instances are bimodal**: every binary draws from the same two
   modes, ~9.20M and ~8.55M (-7% apart). Which mode an instance lands
   in is a per-instance draw (page placement / IRQ / core-layout luck
   — the exact noise axis the perfgate header documents). Median-of-3
   between two modes = a coin flip: v2.0.20 drew 2H+1L (pass), v2.1
   drew 1H+2L (fail), v2.0.21 drew 3L (fail). Pooled across binaries
   there is **no binary→mode correlation** — no evidence that either
   the v2.0.21 hotfix or the v2.1 train moved this angle.
2. **The baseline is stale in both directions**: recorded 2026-06-11
   (~40 releases ago, pre-v1.23-perf-sprint numbers never re-recorded
   despite deliberate improvements — pinned angles now +19% over it),
   while today even the HIGH mode (9.20M) sits -8% under the recorded
   9.98M, putting the floor (baseline×0.92) exactly between today's
   two modes — maximal flakiness by construction.
3. Anti-pattern §1 check: "single run shows -X% loss" — 9 instances
   across 3 binaries say noise/bimodal + stale baseline; declaring a
   v2.1 regression from the first red median would have been that
   anti-pattern.

## Control experiment — baseline-era binary (4ecd017, 2026-06-11) run today

Purpose: distinguish (a) box/environment drift since 06-11 (control
scores ~today's modes → the -8% is the box, re-record baseline
honestly) from (b) real code decay somewhere in v1.15→v2.0.20
(control scores ~9.9M → open a decay hunt; still not a v2.1 issue).

**Result**: **the box has NOT drifted — the gap is real.**

| Binary | legacy_8sh_set instances | median |
|---|---|---|
| 4ecd017 (baseline-era, plain `release` profile as recorded) | [9,986,727 · 9,976,764 · 9,973,447] | **9,976,764** |

The old binary reproduces its 2026-06-11 baseline **to 0.03%** today —
tight, unimodal, on the same box, same hour as the bimodal 8.55/9.20M
runs above. Its other angles also reproduce their recorded baselines
(pinned_cluster_get 25.80M vs 25.57M recorded; legacy_8sh_get 10.88M,
above baseline). Conclusions:

1. Box/environment drift: **refuted**.
2. The legacy_8sh_set decay (-8% high-mode, -14% low-mode) and the
   **bimodality itself** are properties of the NEW code (somewhere in
   v1.15 → v2.0.20, ~40 releases) — the old binary shows neither.
3. Still not a v2.1-branch regression (v2.1 ≡ v2.0.21 ≡ v2.0.20 modes).

## Live confound being tested — build profile

The 06-11 baseline binary was plain `release` (the `release-perf`
profile didn't exist at 4ecd017); all recent gate runs use
`release-perf` (= release + `debug=line-tables-only` + `strip=false`).
Debug info should not change codegen, but the comparison has never
been controlled. Test: v2.0.20 rebuilt with plain `release`, same
gate. If it scores ~9.97M unimodal → the whole "decay" is a
profile-measurement artifact and perfgate must pin one profile; if it
stays bimodal/low → real code decay, bisect across the 40 releases
(clean predicate: any instance < 9.5M = decayed; the two mode bands
are separated by 8%).

**Result**: **profile confound refuted.** Plain-`release` v2.0.20:
[9,194,032 · 8,544,611 · 8,591,542] — same modes as `release-perf`.
(Bonus: its legacy_8sh_get drew [9,970,132 · 9,202,493 · 9,213,798] —
one instance at the old-binary level, so the GET angle is bimodal too.)

## Bisect attempt — converged on noise (kept as a methods lesson)

Automated `git bisect run` between 4ecd017 (good) and v2.0.20 (bad),
probe = plain-release build + legacy_8sh_set × 2 instances, predicate
"any instance < 9.5M = bad", 140 revisions / 8 steps:

```
2834000 (v1.25 A.3 bio thread)      [7,970,278 · 8,002,646]  bad
1fa1b27                              [8,542,177 · 7,466,433]  bad
19ccb7e                              [8,554,357 · 9,973,447]  bad (mixed!)
d795d67                              [9,973,447 · 9,976,764]  good
1361681 (06-15 docs merge)           [9,219,462 · 9,208,143]  bad
bc3acd4 (ClusterClient publish)      [9,222,296 · 9,210,970]  bad
3c14109 (cluster_bench example)      [9,990,053 · 9,986,727]  good
5f86310 (ClusterClient collections)  [9,208,143 · 8,549,481]  → "first bad"
```

**5f86310 is impossible as a culprit** — it touches only
`kevy-client` (+ a test file); the server binary does not link
kevy-client. The convergence is a noisy-bisect artifact, and the
history explains why: instances do not come from two clean modes but
from **at least four attractors** (~9.98M / ~9.21M / ~8.55M /
~7.5–8.0M) whose mixture weights shift gradually across eras (9.2M
draws appear by 06-15; 8.0M draws in the v1.25 era). A single-culprit
step function does not exist; the <9.5M predicate misclassifies the
9.2M mode and bisect chases per-instance luck.

## Verdict

1. **Not a v2.1 regression** — v2.1 ≡ v2.0.21 ≡ v2.0.20 within the
   mixture; every other angle equal or far above baseline.
2. **Real, gradual, multi-step decay** of the legacy_8sh angles
   (≈ -8% typical draw, worst mode -14%) accumulated across
   2026-06-11 → 2026-06-30 (~40 releases, most shipped without a
   perfgate run), plus a new per-instance mode instability the
   06-11 code did not have.
3. The stale baseline compounds it: pinned angles run +19% ABOVE the
   recorded baseline (v1.23 sprint gains never re-recorded), while
   the legacy floor now sits inside today's mode mixture — a gate
   that is simultaneously too loose and a coin flip.

## Actions

- **Next (methodology-correct)**: Phase A decomposition, not more
  bisect — `perf record` old (4ecd017) vs new (v2.0.20) binaries on
  the exact legacy_8sh_set workload, symbol-level diff of where the
  cycles went, plus a mode-attractor study (what per-instance state
  differs between a 9.2M and an 8.5M run of the SAME binary). Own
  campaign, ROADMAP-tracked.
- perfgate hardening (with the campaign): per-angle instance count on
  the legacy angles (bimodality needs n>3), and baseline re-record
  ONLY after the decay is understood — never to green the gate.
- v2.1 release: blocked by the iron rule while the gate is red;
  the decay is proven pre-existing and unrelated — ship/hold is the
  user's call, documented in the session report.

