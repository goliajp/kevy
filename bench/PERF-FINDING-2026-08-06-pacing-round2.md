# Pacing, two rounds in: the uniform gate is dead, the split is half right

Round 1 (uniform occupancy-delta gate on spans + generation gate on the
large pool) was refuted in one line: **M3 came back at 2.40× — glibc's
own number**. B6 measures peak RSS *during* demote churn, and under
sustained churn the delta gate never opens, so nothing returned while
the measurement ran. **M3's 1.98× was never an idle-state floor; it is
mid-churn page return** — the same returns the throughput tax prices.
The RFC's §2 claim ("M3 requires boundedness, not eagerness") is wrong
for the span domain, and the zadd wedge recurred under the gate (same
signature as no-reclaim), so retained span state at scale also breaks
liveness. Two masters, opposite treatment, same pages, same moment: no
uniform WHEN exists.

Round 2 split by domain — spans eager again (M3's mechanism restored
untouched), pool keeps the generation gate — and the battery answered
with three gains and one correction:

| line | eager (morning) | uniform gate | **split** | target |
|---|---:|---:|---:|---|
| liveness | full run | zadd wedge | **full run** | full run |
| lpush | −8.5 (red) | — | **−6.0 (green)** | ≥ 0.92 |
| hset | −17.0 | — | **−13.1** | ≥ 0.92 |
| zadd | −18.4 | — | **−14.3** | ≥ 0.92 |
| sadd | −15.7 | — | −17.1 | ≥ 0.92 |
| pubsub | 0.83–0.84 | 0.841 | **0.881** | ≥ 0.92 |
| M3 | **1.98×** | 2.40× | **2.16×** | ≤ 1.98× |

(perfgate itself flagged box drift vs the recorded baseline, so
cross-run angle deltas are softer than same-run pairs; the M2/M3 pairs
are same-run.)

## The correction

The split's commit claimed pool retention "costs M3 nothing because
B6's 400 B values never touch the pool." **Measured: wrong.** M3 pays
0.18× (RSS peak 737 vs 705 MB) — something large rides the pool during
the demote churn (the demote batch path is the suspect), and 64
drain-generations of retention holds ~80 MB of it through the peak. The
pool *is* pub/sub's refault domain (the +4–5 pp there confirms it), but
it is not disjoint from M3's shape.

## Where this leaves the design

Not accepted — the RFC's own criteria hold: M3 must sit at 1.98× to
the digit and M2 at 0.92. Open moves, in decomposition order rather
than tuning order:

1. **Name the pool's B6 tenant** (perf/probe the demote path's large
   allocations). If the demote batch is the tenant, domain-splitting
   *within* the pool (retain conn/delivery-class sizes, pass demote
   buffers through) is a structural fix; tuning POOL_AGE_DRAINS down is
   the polish version and goes second.
2. **Re-measure clear_page share** under the split: is pub/sub's
   remaining 4 pp still refault, or has the residual moved again?
3. **sadd's −17.1** moved the wrong way while its siblings improved —
   worth one look at whether its allocation sizes straddle the pool
   threshold.

Per the methodology's own rule this is the last hypothesis round before
the next full decomposition: two design rounds have run, both moved
needles double-digits, neither reached the floors.
