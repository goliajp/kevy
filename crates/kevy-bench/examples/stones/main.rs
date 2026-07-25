//! Six-stone microbench baseline runner.
//!
//! Exercises the public API of the six high-blast-radius stone crates
//! (kevy-map / kevy-ring / kevy-config / kevy-text / kevy-store /
//! kevy-vector) with fixed-seed data and reports median ± stdev per
//! operation. Numbers feed `bench/STONE-BENCH.md`.
//!
//! ```text
//! cargo run -p kevy-bench --release --example stones            # all six
//! cargo run -p kevy-bench --release --example stones -- map vector
//! ```
//!
//! Discipline (matches the harness doc): every figure is a median over
//! N ≥ 5 samples after an untimed warm-up pass; stdev is the sample
//! standard deviation over the same samples. Absolute ns drift with host
//! load — treat cross-machine numbers as separate baselines.

mod rng;
mod s_config;
mod s_map;
mod s_ring;
mod s_store;
mod s_text;
mod s_vector;

use kevy_bench::Stats;

/// Print one baseline row. `per` is the number of API operations executed
/// per harness iteration; all figures are divided by it so the row reads
/// as per-operation cost.
fn row(label: &str, s: Stats, per: usize) {
    let d = per as f64;
    println!(
        "  {label:<46} median {:>12.1} ns/op  ± {:>10.1}  p95 {:>12.1}  min {:>12.1}  (N={})",
        s.median_ns as f64 / d,
        s.stdev_ns as f64 / d,
        s.p95_ns as f64 / d,
        s.min_ns as f64 / d,
        s.samples,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let want = |name: &str| args.is_empty() || args.iter().any(|a| a == name);

    println!(
        "six-stone microbench baseline — {} {} (medians are per-op; ± is sample stdev)\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    if want("map") {
        s_map::run();
    }
    if want("ring") {
        s_ring::run();
    }
    if want("config") {
        s_config::run();
    }
    if want("text") {
        s_text::run();
    }
    if want("store") {
        s_store::run();
    }
    if want("vector") {
        s_vector::run();
    }
}
