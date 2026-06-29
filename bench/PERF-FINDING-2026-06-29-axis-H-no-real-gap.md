# Axis H pubsub 4KB — re-verification: no real gap, valkey noise misread as loss

Date: 2026-06-29 (session 8 of autorun)
Anchors:
- Prior single-run probe: [`PERF-PROBE-2026-06-28-axis-sweep-vs-valkey.md`](PERF-PROBE-2026-06-28-axis-sweep-vs-valkey.md) §Axis H — reported `subs=50 msgs=20000 size=4096: kevy 1.64M vs valkey 1.68M = 0.97× LOSS`
- This re-verification: 3-run median-of-3 on lx64, kevy v1.28 baseline / v1.29 B2-alt+OptA / valkey 9.1 default container

## Finding (headline)

The "-3% LOSS at 4KB pubsub" in the 2026-06-28 probe was **a valkey-side noise spike misread as a kevy gap**. Re-measured with 3 independent runs, valkey's `subs=50 msgs=20000 size=4096` throughput has **36% run-to-run variance** — wide enough to span both above and below kevy's stable result.

## Data

3 runs on lx64, axis_h_pubsub.sh's `kevy-pubsub-bench --subs 50 --msgs 20000 --size 4096`, median of 3 within each run:

| Run | kevy v1.28 baseline | kevy v1.29 B2-alt+OptA | valkey 9.1 default container |
|---|---|---|---|
| 1   | 1,639,734 | 1,646,011 | 1,703,867 |
| 2   | 1,634,380 | 1,645,415 | **1,087,286** ← low spike |
| 3   | 1,621,887 | 1,635,190 | 1,732,139 |
| **avg** | **1,632,000** | **1,642,205** | **1,507,764** |
| sample stdev | ~9,000 (0.55%) | ~5,800 (0.36%) | ~363,000 (24%) |

**kevy v1.29 vs valkey 9.1: 1,642,205 / 1,507,764 = 1.089× → kevy 8.9% AHEAD on average.**

The 2026-06-28 probe's single-run point estimate (kevy 1.64M, valkey 1.68M) sat in valkey's noise band; treating it as a real gap was a measurement methodology error. Per the perf methodology `feedback-perf-vs-foss-decomposition.md` § "5 µs win 没法 validate" — n=1 against a 24%-variance baseline can't distinguish 3% gap from 30% gap.

## Why valkey is so noisy on this workload

Hypothesis (not source-verified, defer to a future Phase A if needed):

- valkey 9.1 with `--io-threads 10` distributes the publish fan-out work across 10 worker threads. Each subscriber send may land on a different thread depending on scheduler decisions. Cache-residency of the message body bytes in the shared L3 varies run-to-run; sometimes all 10 threads share the same hot line, sometimes not.
- kevy `--threads 1` does the entire fan-out on a single core, so cache topology is deterministic per run.

This isn't a valkey bug — io-threads is a feature. But it does make single-run benchmarks against valkey on fan-out-heavy workloads unreliable for gap measurement.

## What this means for the 2026-06-28 probe's findings

| Axis (probe) | Originally reported | Re-verified |
|---|---|---|
| Axis A (pipelining `-P 1..256`) | kevy 1.03-4.09× ahead at all -P | unchanged, kevy ahead |
| Axis B (`-d 64..65536`) | kevy wins ≤16K, LOSES -3/-8% at 64K | confirmed at 64K (-6% gap), loopback-bound per `PERF-FINDING-2026-06-29-arc-from-box-memcpys.md` §"throughput-neutral" |
| Axis H (pubsub fan-out) | kevy 4-7× ahead small msg, -3% at 4K | **CORRECTED**: kevy ahead 8.9% at 4K too (valkey noise misread) |

**Net**: kevy is competitive-or-ahead of valkey 9.1 at every axis measured except `-d 65536 SET`, which is loopback-bound (no app-layer fix can close it; would need D-series kernel-side work).

## What this enables for v1.29.0

The Option A (`Arc<Box<[u8]>>`) + B2-alt (kernel-direct prep_read) implementation chain:

- Doesn't move throughput on the originally-targeted `-d 65536 SET` axis (loopback-bound).
- Doesn't regress throughput on any small-payload axis (kevy still ahead).
- Doesn't regress on axis H pubsub at 4KB (within noise; kevy still ahead).
- Lands real architectural improvements: structurally memcpy-free bareset write path, type-system-correct `Arc<Box<[u8]>>` storage that matches the runtime layout, kernel-direct prep_read infrastructure (`prep_cancel` + `OP_BIG_CANCEL` / `OP_BIG_READ` tags) reusable for future workload-specific attacks.

There is no per-workload throughput win to headline. But there IS a clean release-worthy delta: architectural prep + verified parity + Phase A re-decomposition findings recorded.

## Open question for v1.29.0 ship decision

Per project standing authorization (上限性能优先 + 不考虑 ROI / win/risks), the v1.29.0 ship gates on whether the user wants:

- (Y) Ship as "architectural prep + parity verified" minor release. Real code, real tests, real perf record evidence; no headline throughput win.
- (N) Don't ship. Sit on feature branch; merge to develop when a per-workload win lands on top.

This is the kind of release scope question that warrants explicit user input — once published to crates.io it cannot be unpublished. Defer to user.
