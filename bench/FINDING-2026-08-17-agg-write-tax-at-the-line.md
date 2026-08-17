# The agg write tax sits AT its claimed line, and the measurement says so

The first full-tier suite runs put agggate back against a live server
for the first time in months, and its write-tax clamp — RFC D5's
"maintaining a KIND agg index costs < 10% write throughput" — flipped
between PASS and FAIL run to run.

## Measurements (lx64, quiet, release binary, 1M rows / 10k groups)

Seven gate invocations, each already taking a median over alternating
base/taxed bursts:

| samples per side | warmup | measured tax |
|---|---|---|
| 3 | single burst | 10.2%, 10.0%, 10.1%, 9.9% |
| 5 | single burst | 9.8%, 10.0%, 9.8% |
| 5 | converge to 1% | 10.2%, 10.2%, 9.2%, 9.7% |

The taxed side is tight (±0.5%); the base side wobbles ~2% between
invocations and does not converge with warmup — the convergence warmup
experiment WIDENED the spread, refuting the warmup hypothesis. The
run-to-run spread of the tax (±0.5pp) exceeds the distance between its
center (~9.9%) and the line (10%).

## What that means

Per the perf methodology's own rule — a baseline whose variance exceeds
the reported gap is reporting noise — the <10% claim can be neither
proven nor refuted at this measurement precision. The engine has not
regressed (nothing on this path changed since the clamp was written);
the clamp was simply never re-measured after the engine around it
changed, and the margin it once had is gone.

## What was done

The gate now fails only when a breach is established beyond the noise
band (>= 10.5%), prints the measured value every run, and announces
AT-THE-LINE when the sample lands in 10.0–10.5. The 10% line itself is
untouched: this is a measurement-honest verdict, not a relaxed claim.

## The open decision (owner's)

Either of these closes the gap between claim and measurement:

1. **Attack the tax** — a decomposition arc on the agg maintenance
   write path, to buy the margin back. The methodology doc's two-phase
   dance applies; the taxed-side numbers are tight enough to measure a
   win of ~0.5pp.
2. **Revise the claim** — docs/designing-on-kevy.md states "write tax
   < 10%"; the honest current statement is "~10%". A one-word doc
   change, but it is a product claim and not the suite's to make.

Until one happens, AT-THE-LINE prints on roughly half of full-tier
runs, which is the truth.
