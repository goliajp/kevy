# T9 L3 finding — the kernel already batches per conn; there is nothing to group

Verdict: L3 (per-CQE -> per-conn batching in `run_uring`) is REFUSED
at the pre-Phase-B gate. Zero code changed; the knife never landed.
The premise — "one conn often has several recv CQEs in one reap, so
per-CQE conn loading / flush bookkeeping is paid repeatedly" — was
measured and is empirically false.

## Gate instrument

A temporary (uncommitted) counter probe in `run_uring` /
`uring_arm_conns` on the bench box: per-reap recv-CQE conn-duplicate
accounting + reap/CQE/arm-visit rates, 5 s windows, lx64, release-perf,
`-P 16 --threads 8` continuous load. Steady-state windows:

| shape | recv CQE/s | recv_dup | recv/reap | comps/reap | arm visits/recv |
|---|---:|---:|---:|---:|---:|
| c50 GET  | 503k | **0** | 1.17 | 2.35 | 2.87 |
| c100 GET | 550k | **0** | 1.75 | 3.50 | 2.79 |
| c50 SET  | 374k | **0** | 1.57 | 3.14 | 2.83 |

Across ~33M recv CQEs in 15 windows the same conn appeared twice in
one reap batch exactly **zero** times. Multi-recv reaps do occur
(c100: 1.75 recv/reap), so the counter had every chance to fire — the
CQEs in a batch are always *distinct* conns. Grouping by conn groups
nothing.

## Why the decomp's +10-18% evaporates

- At P16 one recv CQE already carries ~12.7 ops (6.39M ops/s over
  503k CQE/s — a 16-op pipeline burst lands as ~1.26 CQEs). Every
  per-CQE cost L3 wanted to batch is already amortized 12.7x by the
  kernel + provided-buffer ring. The whole per-CQE fixed surface
  (2-3 map probes + input take/restore + mark_arm_pending, ~90-150
  cycles/CQE by the cost table) is ~7-12 cycles/op = **0.15-0.25%**
  of the 4,700 c/op budget — two orders of magnitude under the 5pp
  gate bar, before any grouping-overhead cost.
- The decomp's "run_uring self 24.5%" was the c100 get+set-mix
  aggregate. A clean-binary perf record on the c50 GET axis shows
  run_uring self at **9.15%**, and its bulk is the per-iter busy-poll
  body (spin / arm scan / submit) — the surface L2 already ruled
  "productive waiting", not per-CQE repetition.
- Median-of-5 re-measure (clean binary, n=8M): c50 GET is **bimodal**
  — 6.39M x3 / 7.98M x2 in the same session; c100 GET sat at 6.38M
  x5 (the decomp's "c100 = 8.0M plateau" did not reproduce). The
  8M mode engages or not per run regardless of conn count, which
  further supports the L2 re-read (a joint client-side / placement
  regime, not a server conn-density effect) and would make a +5%
  landed-gate unresolvable for a sub-1% attack anyway.

## Residue (archived, not acted on)

The unconditional post-write-CQE `mark_arm_pending` visit makes the
arm loop run ~2.9 visits per burst where ~2 carry work; eliding the
empty one is worth ~4-8 cycles/op — sub-noise, and it rides the
hottest loop in the binary (codegen-risk > payoff). Noted only so a
future reader does not re-derive it as a lever.

Next lever by the data: L4 (map/store layout for the spread axis) or
the L5 io_uring polish basket; the T9 lever table's only step-change
candidate (L1) stays REFUSED per its own gate.
