# K4 premise check: the corpus-vs-datum claim, measured before the crate exists

**Verdict: the premise LIVES.** Cross-value redundancy is real, a
per-datum baseline provably cannot reach it, and the "O(dictionary) +
N × small" shape K4 asserts is literally observable. T3 can be built
against these numbers as its corpus baseline. One adversarial edge got
sharper: on incompressible input a naive per-datum encoder *expands*,
so K2's early-exit is a requirement confirmed by measurement, not by
caution.

## Why this ran now

`bench/compressgate.sh` (r1-locality) says K4 "is the line that says
whether this design was worth doing — if K4 cannot be made to pass,
that is the finding, and it retires the train." Three of R4c's five
deliverables had their premises overturned at start-of-work. This train
has cost zero implementation lines so far, which is the cheapest a
premise check will ever be.

## Method

Research instruments only — `zlib` as the oracle (the zero-dep law
binds the product, not the lab bench). Four corpora, N=1000 values of
400 B each; three encoders per corpus:

- **per-datum** — each value compressed alone: the baseline K4 says
  cannot pass;
- **shared-dict** — each value compressed against a shared 32 KiB
  dictionary sampled from the corpus, the dictionary's bytes counted in
  full;
- **segment** — the whole corpus as one stream: the ceiling for any
  cross-value capture.

Script: `bench/k4_premise.py`, checked in beside this doc so the
first consumer-shaped corpus can be measured the same way.

## Numbers (bytes per value; ratio = raw/encoded)

| corpus | per-datum | shared-dict | segment |
|---|---:|---:|---:|
| identical (K4's literal shape) | 89.0 (4.49×) | **41.8** (9.58×) | 2.4 (168.9×) |
| templated JSON (realistic rows) | 231.6 (1.73×) | 180.0 (2.22×) | 128.9 (3.10×) |
| random (K2's adversarial shape) | **411.0 (0.97×)** | 405.7 | 400.1 |
| textual (shared vocabulary) | 148.8 (2.69×) | 103.8 (3.85×) | 58.5 (6.83×) |

## What each row settles

1. **K4's structural claim holds.** On the identical corpus the
   shared-dict total is 32 768 (dictionary) + ~9 B × N — *exactly* the
   "O(dictionary) + N × small" shape — while per-datum pays 89 B for
   every copy forever. No tuning of a per-datum encoder changes that:
   the redundancy is *between* values, and a per-datum window cannot
   see between values.

2. **The realistic gap is worth having.** On templated JSON rows,
   per-datum leaves 44 % of the reachable bytes on the table
   (231.6 vs the 128.9 ceiling). The corpus effect is not an artifact
   of the identical-values toy.

3. **K2's early-exit is measured, not hypothetical.** Per-datum zlib on
   random 400 B values emits **411 B/value** — an 11-byte expansion per
   record. "Never expands" therefore requires the incompressible
   early-exit path in the frame format; it cannot be had for free from
   the codec.

4. **A quantified design tension for T3, stated before it is built.**
   The segment column is the ceiling, but kevy's cold reads are
   per-record (`read_at` → decode one value inside the K1 p99 budget),
   so segment-stream encoding is not reachable — the dictionary is the
   mechanism that must carry K4. The 32 KiB sampled dict captures 72 %
   of the ceiling's ratio on realistic rows (2.22× of 3.10×) and only
   6 % of it on the identical corpus (9.58× of 168.9×) — dictionary
   construction, not match-finding, is where the capture is won or
   lost. That is where T3's design attention should go first.

## What this does not settle

Throughput (K1's decode-must-be-memcpy-class) is untouched — zlib is an
oracle for ratios, not for speed. And these corpora are synthetic;
the first consumer-shaped corpus (mailrs values) should be measured the
same way when T3 opens.
