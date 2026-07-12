# PERF-FINDING 2026-07-12 — every threaded redis-benchmark number we have ever recorded is quantized to a 250 ms grid

**Status**: root cause proven from source + reproduced across four unrelated
configurations. Invalidates the *precision* of the arena table, the perfgate
`legacy_8sh_*` baselines, and three prior findings that reasoned about
"instance modes" and "regressions" inside the grid.

---

## The mechanism (redis-benchmark.c, redis 8.0)

| line | code | consequence |
|---|---|---|
| `:52` | `#define SHOW_THROUGHPUT_INTERVAL 250 /* 250ms */` | the timer period |
| `:1653` | `if (config.num_threads && requests_finished >= config.requests) { aeStop(eventLoop); return AE_NOMORE; }` | **with `--threads`, the only place the benchmark stops is inside that 250 ms timer** |
| `:425` | `if (!config.num_threads && config.el) aeStop(config.el);` | without `--threads`, `clientDone` stops it the instant the last reply lands |
| `:970`, `:973` | `config.start = mstime(); … config.totlatency = mstime()-config.start;` | elapsed is measured until `aeMain` returns — i.e. until the timer fires |

So under `--threads`, the run keeps the event loop alive, doing nothing, until
the next 250 ms tick. The reported `requests per second` is
`requests / totlatency`, and **`totlatency` is therefore rounded UP to a
multiple of 250 ms**.

Two consequences:

1. **Throughput is quantized.** The only reportable values are `N / (k·250ms)`.
2. **Throughput is systematically UNDER-reported**, because the rounding is
   always up in time.

The relative width of one bucket is `250ms / elapsed`. At the arena's ~1.25 s
that is **20%**. At perfgate's ~3.5 s it is **7%**.

## The evidence

Every level we have ever called an "instance mode" or an "attractor" sits on
the grid. Elapsed = N / reported-rps:

| configuration | reported rps | elapsed |
|---|---:|---:|
| perfgate legacy_8sh_set, N=30M | 9,213,798 | **3.2560 s** |
| | 8,554,357 | **3.5070 s** |
| | 7,983,004 | **3.7580 s** |
| same, N=20M | 8,873,115 | **2.2540 s** |
| | 7,984,032 | **2.5050 s** |
| same, `--accept-shards 1` | 5,696,383 | **3.5110 s** |
| | 5,316,321 | **3.7620 s** |
| | 4,985,045 | **4.0120 s** |
| arena (N=8M, -P 16) GET | 6,389,776 | **1.2520 s** |
| arena SET | 5,326,232 | **1.5020 s** |
| arena HSET | 3,998,001 | **2.0010 s** |
| arena LPUSH | 3,196,164 | **2.5030 s** |
| arena ZADD | 2,666,667 | **3.0000 s** |
| arena valkey GET | 2,131,628 | **3.7530 s** |
| arena valkey HSET | 1,776,199 | **4.5040 s** |
| arena valkey SET | 1,598,082 | **5.0060 s** |

Steps of **0.250–0.251 s**, in every configuration, for kevy and for valkey
alike. Nothing else in the system produces a 250 ms constant.

Drop `--threads` and the grid disappears (same box, same server, N=20M,
6 fresh instances each):

```
--threads 8   8869179  7971303  8869179  7955450  8000717  8869179   <- two values, repeating
no --threads  9433963  9886307  9891196 10152284  9760859  9647854   <- continuous, all distinct
```

The unthreaded readings are also **6–14% higher**, exactly as the "rounds
elapsed up" model predicts.

## What this invalidates

* **K-402's "v4 SET write-path regression, -18.7%, distributions disjoint"**
  (`PERF-FINDING-2026-07-11-v4-set-write-path-regression.md`). The two
  binaries landed in different buckets. Re-measured today, order-balanced,
  n≈44 fresh instances each: **medians 8,556,796 (v3.18.0) vs 8,554,357 (v4)
  — a 0.03% difference**, ranges identical, both binaries visiting all three
  buckets. There is no regression.
* **K-401's "LPUSH instance-level bimodality" (2.91M / 3.20M).** Elapsed
  2.7530 s and 2.5030 s — adjacent buckets. The conclusion ("not a
  regression") was right; the stated reason was not.
* **The "8.55M attractor"** of the 2026-07-03 bimodal study. A bucket.
* **The perfgate `legacy_8sh_set` baseline, 9,210,970.** That is the *lucky*
  bucket — v3.18.0 itself only lands there in ~40% of fresh instances, so the
  gate red-lights its own baseline binary most of the time. It was never a
  measurement of the code.
* **The arena table's precision.** GET and SET both reading exactly 6,389,776,
  and INCR and SADD both reading exactly 5,326,232, is not a coincidence and
  not a transcription error: they are the same bucket. The true values are
  somewhere between their bucket and the next one up, and are **understated**.
  The ratios vs valkey survive in order-of-magnitude terms (valkey is quantized
  too, and is three to five buckets away) but the two-decimal ratios do not.
* **My own G2 "environmental false-positive" verdict** (2026-07-12, retracted
  earlier today for a different reason — it compared two feature/v4 commits).
  Its conclusion was close to correct by accident.

## What it does NOT invalidate

The **direction** of every headline claim. kevy is three to five buckets away
from valkey on every arena face; no amount of 250 ms rounding closes a 3×
gap. What dies is the *precision* — the "3.00×", the "±2.8k", the "-18.7%".

## The fix

Both faces of the measurement have to stop reading a rounded clock.

1. **perfgate** — drop `--threads` where the client can still saturate, or
   read throughput from the server's own command counter over a wall interval
   sampled mid-run. Server-side counting is immune to the client's stop path
   and is the orthodox answer for a self-vs-self ratchet.
2. **arena** — the competitor comparison must use a client whose stop is not
   timer-bound, or a run long enough that 250 ms is below the gate's
   tolerance. At 8M requests and ~1.25 s, the bucket is 20% wide: the current
   protocol cannot resolve anything finer than "kevy is much faster".
3. **Re-record every baseline** with the fixed harness. This is not "changing
   the books to get a green light" — the recorded numbers were never
   measurements of the code in the first place.

## The methodology lesson

The `bench/perfgate.sh` header has warned since 2026-06-11 that instance
variance is "the dominant noise axis". It was not noise. It was the ruler.

Three separate investigations (K-401, K-402, G2) each looked straight at the
grid — K-402's own doc even flagged that its 8.56M reading "coincides with the
historical 8.55M attractor" — and each of them theorised about *the system*
instead of asking what could possibly produce a constant 250 ms step. The
question "what has a 250 ms period?" was one arithmetic operation away
(`N / rps`) for four straight days.

**When measurements cluster on evenly-spaced levels, suspect the instrument
before the system.** Added to `.claude/rule/perf-vs-foss.md` as R13.
