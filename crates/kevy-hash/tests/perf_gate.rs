//! Per-op ns budget gates. Trip = regression beyond host-noise.
//!
//! Reproducer: `cargo test -p kevy-hash --test perf_gate --release`

use kevy_bench::{bench, black_box};
use kevy_hash::{FxHasher, KevyHash};
use std::hash::Hasher;

/// 16-byte byte-string hash via `KevyHash` trait (one-call). Budget = 50 ns
/// (conservative for loaded host; cache-hot should be ~5-10 ns).
#[test]
fn kevy_hash_bytes_under_budget() {
    let buf: Vec<u8> = (0..16u8).collect();
    let s = bench(40, 100_000, || {
        black_box(black_box(buf.as_slice()).kevy_hash());
    });
    assert!(
        s.median_ns < 50,
        "KevyHash bytes[16] median = {} ns, budget 50",
        s.median_ns
    );
}

/// u64 hash via `KevyHash`. Budget = 20 ns (one mix + fmix64; sub-5ns warm).
#[test]
fn kevy_hash_u64_under_budget() {
    let s = bench(40, 100_000, || {
        black_box(black_box(0xdead_beef_u64).kevy_hash());
    });
    assert!(
        s.median_ns < 20,
        "KevyHash u64 median = {} ns, budget 20",
        s.median_ns
    );
}

/// FxHasher state-machine path (used by std::HashMap callers). Budget = 70 ns.
#[test]
fn fxhasher_bytes_under_budget() {
    let buf: Vec<u8> = (0..16u8).collect();
    let s = bench(40, 100_000, || {
        let mut h = FxHasher::default();
        h.write(black_box(buf.as_slice()));
        black_box(h.finish());
    });
    assert!(
        s.median_ns < 70,
        "FxHasher bytes[16] median = {} ns, budget 70",
        s.median_ns
    );
}
