//! kevy-bench — a pure-Rust micro-benchmark harness. Zero dependencies.
//!
//! kevy is 0-dependency by charter, so we cannot pull `criterion`. This is the
//! self-hosted equivalent, used by two callers:
//!
//! * per-crate exploration benches (`examples/bench_*.rs`) — compare two
//!   implementations head-to-head and print a speedup ratio. This ratio is the
//!   evidence the dep/std self-host evaluation is gated on ("if the candidate
//!   isn't ≥ N× the incumbent, don't self-host it").
//! * per-crate regression gates (`tests/perf_gate.rs`) — assert a hot path stays
//!   under a documented budget (see [`time_median`]).
//!
//! # Why the ratio survives a loaded host
//!
//! The campaign that birthed this crate could not get a clean full-system
//! throughput number because the dev host is permanently busy. The escape is to
//! measure *components*, not the whole server: run the variants **back-to-back
//! in one process**. Both pay the same contention in the same window, so the
//! absolute ns drifts with load but the **ratio between them holds**. Treat
//! `median_ns` as relative, not absolute, unless the host is known idle.
//!
//! # Method
//!
//! Each *sample* times `inner` iterations of the closure and divides, so the
//! per-op figure isn't swamped by `Instant::now()` overhead (~tens of ns) for
//! sub-microsecond work. We report the **median** sample (robust to the
//! occasional scheduler hiccup) plus p95 and min.
//!
//! ```
//! use kevy_bench::{bench, black_box};
//!
//! let s = bench(50, 1000, || { black_box(1u64 + black_box(2)); });
//! assert!(s.median_ns < 1_000); // trivial add is nanoseconds
//! ```
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

/// Re-exported so benches can fence the optimizer without importing `std::hint`.
pub use std::hint::black_box;

/// Per-operation timing summary, in nanoseconds. See [`bench`].
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// Number of samples collected.
    pub samples: usize,
    /// Iterations timed per sample (the divisor applied to each sample).
    pub inner: usize,
    /// Fastest sample — the least-disturbed run, closest to the true cost.
    pub min_ns: u64,
    /// Median sample — the headline figure, robust to occasional hiccups.
    pub median_ns: u64,
    /// 95th-percentile sample — tail behaviour under scheduler noise.
    pub p95_ns: u64,
    /// Mean across all samples.
    pub mean_ns: u64,
}

/// Run `op` and return per-operation timing stats.
///
/// `samples` outer repetitions, each timing `inner` calls of `op` and dividing.
/// One untimed warm-up sample primes caches/branch predictors first. Pick
/// `inner` large enough that one sample is comfortably above `Instant`
/// resolution (≥ a few µs); for ns-scale ops use `inner` in the thousands.
pub fn bench<F: FnMut()>(samples: usize, inner: usize, mut op: F) -> Stats {
    assert!(samples > 0 && inner > 0, "samples and inner must be non-zero");

    // Warm-up: not recorded.
    for _ in 0..inner {
        op();
    }

    let mut per_op: Vec<u64> = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        for _ in 0..inner {
            op();
        }
        let elapsed = start.elapsed();
        per_op.push(elapsed.as_nanos() as u64 / inner as u64);
    }
    per_op.sort_unstable();

    let sum: u64 = per_op.iter().sum();
    Stats {
        samples,
        inner,
        min_ns: per_op[0],
        median_ns: per_op[samples / 2],
        p95_ns: per_op[((samples * 95) / 100).min(samples - 1)],
        mean_ns: sum / samples as u64,
    }
}

/// Median wall-clock time of a single `op` call over `iters` runs.
///
/// The shape `tests/perf_gate.rs` wants: time each call individually, take the
/// median, assert it under a budget. Budgets should carry generous headroom —
/// dev (unoptimised) is typically 5–25× slower than release, and a loaded host
/// adds more — so size the budget off the *release* number times a safety factor
/// and document the observed dev figure alongside it.
pub fn time_median<F: FnMut()>(iters: usize, mut op: F) -> Duration {
    assert!(iters > 0, "iters must be non-zero");
    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        op();
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    samples[iters / 2]
}

/// Print one labelled timing line.
pub fn report(label: &str, s: Stats) {
    println!(
        "  {label:<30} median {:>8} ns   p95 {:>8} ns   min {:>8} ns",
        s.median_ns, s.p95_ns, s.min_ns
    );
}

/// Print both timings and the candidate's speedup over the baseline (by median).
///
/// `ratio = baseline.median / candidate.median`; > 1 means the candidate is
/// faster. This is the number the self-host decision is gated on.
pub fn compare(base_label: &str, base: Stats, cand_label: &str, cand: Stats) -> f64 {
    report(base_label, base);
    report(cand_label, cand);
    let ratio = base.median_ns as f64 / (cand.median_ns.max(1)) as f64;
    let verdict = if ratio >= 1.0 { "faster" } else { "slower" };
    println!("  → {cand_label} is {ratio:.2}× {verdict} than {base_label} (median)\n");
    ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_runs_and_orders_stats() {
        let s = bench(20, 500, || {
            black_box(black_box(2u64).wrapping_mul(black_box(3)));
        });
        assert_eq!(s.samples, 20);
        assert_eq!(s.inner, 500);
        assert!(s.min_ns <= s.median_ns);
        assert!(s.median_ns <= s.p95_ns);
    }

    #[test]
    fn time_median_nonzero() {
        let d = time_median(50, || {
            black_box((0..100).sum::<u64>());
        });
        assert!(d.as_nanos() < 1_000_000);
    }

    #[test]
    fn compare_returns_ratio() {
        let fast = Stats {
            samples: 1,
            inner: 1,
            min_ns: 10,
            median_ns: 10,
            p95_ns: 10,
            mean_ns: 10,
        };
        let slow = Stats {
            median_ns: 40,
            ..fast
        };
        // candidate `fast` vs baseline `slow` → 4× faster.
        let r = compare("slow", slow, "fast", fast);
        assert!((r - 4.0).abs() < 0.01);
    }
}
