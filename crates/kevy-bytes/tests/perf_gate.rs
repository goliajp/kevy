//! Hot-path budget gate for SmallBytes. Asserts the per-op ns stays under
//! a documented ceiling, so a regression trips the test instead of silently
//! shipping. Ceilings are conservative — set high enough to survive load on
//! the dev box, low enough to catch real regressions.
//!
//! Reproducer: `cargo test -p kevy-bytes --test perf_gate --release`
//!
//! These tests measure raw ns and only make sense in release mode. Under
//! debug / coverage instrumentation the budgets are 10-30× off; gate to
//! release.

#![cfg(not(debug_assertions))]

use kevy_bench::{bench, black_box};
use kevy_bytes::SmallBytes;

/// Inline path: from_slice on a ≤22B payload should be a memcpy + tag write,
/// nothing else. Budget = 50 ns (generous; cache-hot it's typically < 10 ns).
#[test]
fn from_slice_inline_under_budget() {
    let buf = b"hello world!".to_vec();
    let s = bench(40, 50_000, || {
        black_box(SmallBytes::from_slice(black_box(&buf)));
    });
    assert!(
        s.median_ns < 50,
        "SmallBytes::from_slice (12B inline) median = {} ns, budget 50",
        s.median_ns
    );
}

/// Cloning an inline SmallBytes is just a 24-byte memcpy + tag. Budget = 50 ns.
#[test]
fn clone_inline_under_budget() {
    let sb = SmallBytes::from_slice(b"hello world!");
    let s = bench(40, 50_000, || {
        black_box(black_box(&sb).clone());
    });
    assert!(
        s.median_ns < 50,
        "SmallBytes clone (12B inline) median = {} ns, budget 50",
        s.median_ns
    );
}

/// as_slice in the inline path is a load + range slice (no heap touch).
/// Budget = 20 ns (very loose; should be sub-5ns warm).
#[test]
fn as_slice_inline_under_budget() {
    let sb = SmallBytes::from_slice(b"hello world!");
    let s = bench(40, 100_000, || {
        black_box(black_box(&sb).as_slice());
    });
    assert!(
        s.median_ns < 20,
        "SmallBytes as_slice (inline) median = {} ns, budget 20",
        s.median_ns
    );
}

/// size + align of the public type are part of the contract — pin them in a
/// test so a misalignment regression is loud (the const_assert at lib.rs:60
/// also catches this at compile time; this test is the runtime fallback for
/// e.g. cross-compile validation).
#[test]
fn size_and_align_pinned() {
    assert_eq!(std::mem::size_of::<SmallBytes>(), 24);
    assert_eq!(
        std::mem::align_of::<SmallBytes>(),
        std::mem::align_of::<usize>()
    );
}
