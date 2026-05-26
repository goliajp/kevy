//! Per-op ns budget gates for kevy-ring's SPSC push/pop. Single-thread same
//! ring (no producer/consumer split) — that's the lower bound on hot path
//! cost; cross-thread adds a cross-core hop the bench in examples/bench_ring.rs
//! measures separately.
//!
//! Reproducer: `cargo test --release -p kevy-ring --test perf_gate`

use kevy_bench::{bench, black_box};
use kevy_ring::ring;

/// Same-thread push+pop on a 256-slot ring. Cache-hot budget = 80 ns/op
/// (push+pop pair); conservative for loaded host.
#[test]
fn push_pop_same_thread_under_budget() {
    let (mut tx, mut rx) = ring::<u64>(256);
    let s = bench(40, 100_000, || {
        tx.push(black_box(7u64)).unwrap();
        black_box(rx.pop());
    });
    assert!(
        s.median_ns < 80,
        "push+pop same-thread median = {} ns, budget 80",
        s.median_ns
    );
}

/// Sized capacity assertion — the ring rounds capacity UP to a power of
/// two; this is a hot-path contract (`& mask` index instead of `% cap`).
#[test]
fn capacity_is_power_of_two() {
    let (tx, _rx) = ring::<u64>(100);
    let c = tx.capacity();
    assert!(c.is_power_of_two(), "capacity {c} not a power of two");
    assert!(c >= 100);
}
