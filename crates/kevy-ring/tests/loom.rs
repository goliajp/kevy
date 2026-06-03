//! Loom enumeration test for the SPSC ring.
//!
//! Loom intercepts atomic ops + thread scheduling and exhaustively explores
//! every legal interleaving of `Producer::push` / `Consumer::pop`, asserting
//! the SPSC invariants hold under *every* observed reordering — not just the
//! ones a stress test happened to hit. This catches the kind of bug that
//! survives a million iterations of a real-thread stress test because the
//! racy interleaving only manifests once in a billion.
//!
//! ## Charter
//!
//! `loom` is a charter-exempted dev-only crate (same status as `cargo-fuzz`
//! and `cargo-llvm-cov`): it never enters the runtime dependency closure of
//! `kevy-ring`. The `[target.'cfg(loom)'.dependencies]` gate in Cargo.toml
//! means it only resolves when building under `--cfg loom`. A normal
//! `cargo build` / `cargo test` resolves the std atomics / UnsafeCell / Arc
//! and the loom crate is not even downloaded.
//!
//! ## How to run
//!
//! ```bash
//! RUSTFLAGS="--cfg loom" cargo test -p kevy-ring --test loom --release
//! ```
//!
//! `--release` is recommended: loom's exhaustive search explores tens of
//! thousands of interleavings, and debug builds make each one painfully slow.
//! Total wall-clock is on the order of 1-10s for the cases below.
//!
//! `LOOM_MAX_PREEMPTIONS` (default 2) bounds how aggressive the preemption
//! search is; bumping to 3+ explodes the state space combinatorially. The
//! invariants here are small enough that the default suffices.

#![allow(unexpected_cfgs)]
#![cfg(loom)]

use kevy_ring::ring;

/// Push N items, pop N items. Loom enumerates every observable interleaving
/// of the producer's `tail.store(Release)` and the consumer's `tail.load(Acquire)`,
/// and the symmetric `head` pair, and asserts:
///
///  - Every push that the API reports as successful corresponds to exactly
///    one observed pop with the same value (no message lost, no duplicated).
///  - Items pop in the exact insertion order (FIFO).
///  - The consumer never reads an uninitialized slot (UB; loom catches this
///    via the underlying UnsafeCell tracking — would panic the model).
#[test]
fn spsc_round_trip_2_items_cap_2() {
    loom::model(|| {
        let (mut tx, mut rx) = ring::<u32>(2);

        let p = loom::thread::spawn(move || {
            // Spin until both pushes succeed (ring full → retry). Under loom
            // every spin schedules a context switch, so this is fast and
            // model-exhaustive even with a tiny capacity.
            while tx.push(10).is_err() {
                loom::thread::yield_now();
            }
            while tx.push(20).is_err() {
                loom::thread::yield_now();
            }
        });

        // Consumer pulls both values in FIFO order.
        let mut seen = Vec::with_capacity(2);
        while seen.len() < 2 {
            if let Some(v) = rx.pop() {
                seen.push(v);
            } else {
                loom::thread::yield_now();
            }
        }
        p.join().unwrap();

        assert_eq!(seen, [10, 20], "FIFO order violated under interleaving");
    });
}

/// Capacity-2 ring with wrap-around: push 3 items (one wraps), pop all 3.
/// Stresses the `tail & mask` slot-index computation across a wrap event,
/// where a naïve impl might race the cached-cursor fast path against the
/// shared store/load on the wrap iteration.
#[test]
fn spsc_wrap_around_at_capacity_2() {
    loom::model(|| {
        let (mut tx, mut rx) = ring::<u8>(2);

        let p = loom::thread::spawn(move || {
            for v in 1u8..=3 {
                while tx.push(v).is_err() {
                    loom::thread::yield_now();
                }
            }
        });

        let mut got = Vec::with_capacity(3);
        while got.len() < 3 {
            if let Some(v) = rx.pop() {
                got.push(v);
            } else {
                loom::thread::yield_now();
            }
        }
        p.join().unwrap();

        assert_eq!(got, [1, 2, 3], "wrap-around lost or reordered items");
    });
}
