//! Fuzz `kevy_ring` — the SPSC ring that carries every cross-shard hop in
//! kevy-rt's thread-per-core runtime.
//!
//! Two phases per input:
//!
//! 1. **Single-thread sequence semantics** vs a `VecDeque` oracle:
//!    push/pop order driven by the input bytes, capacity from the first
//!    byte. Asserts FIFO contents, `Err(val)` hands the value back exactly
//!    when the oracle says the ring is full, `len`/`is_empty`/`is_full`
//!    agree with the oracle after every op, and a final drain matches.
//!
//! 2. **Two-thread stress** (producer thread + consumer on the fuzz
//!    thread): every value 0..n must arrive exactly once, in order —
//!    strict sequence equality implies conservation (nothing lost,
//!    nothing duplicated, nothing reordered). Capacity and n come from
//!    the input; n is kept small (≤ ~2K) so each exec stays fast while
//!    small capacities still force many full/empty transitions.
//!
//! INFRA NOTE: the two-thread phase is capped at a global
//! 100K executions per process. ASAN keeps a ThreadContext (~250 B)
//! for every thread ever created and never recycles it, so unbounded
//! thread-per-exec churn OOMs libFuzzer's default 2 GB rss_limit
//! (observed locally: OOM at exec #8,247,964, rss 2050 MB, ~23K
//! exec/s ≈ 6 min; the oom artifact was a 3-byte input — an infra
//! artifact, not a kevy-ring leak). After the cap, the same
//! conservation check runs single-threaded, and the loom model checker
//! (tests/loom.rs) already covers the interleaving space exhaustively.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Global budget for thread-spawning executions (see INFRA NOTE).
static THREADED_EXECS: AtomicUsize = AtomicUsize::new(0);
const THREADED_EXEC_CAP: usize = 100_000;

fuzz_target!(|data: &[u8]| {
    // ---- Phase 1: single-thread push/pop vs VecDeque oracle ----
    let mut it = data.iter().copied();
    let cap_req = it.next().unwrap_or(0) as usize;
    let (mut tx, mut rx) = kevy_ring::ring::<u64>(cap_req);
    let cap = tx.capacity();
    assert_eq!(
        cap,
        cap_req.max(2).next_power_of_two(),
        "capacity contract broken (documented: power of two, minimum 2)"
    );
    assert_eq!(rx.capacity(), cap);

    let mut oracle: VecDeque<u64> = VecDeque::new();
    let mut seq = 0u64;
    for op in it.by_ref().take(512) {
        if op & 1 == 0 {
            seq += 1;
            match tx.push(seq) {
                Ok(()) => {
                    assert!(oracle.len() < cap, "push succeeded on a full ring");
                    oracle.push_back(seq);
                }
                Err(v) => {
                    assert_eq!(v, seq, "push must hand the rejected value back");
                    assert_eq!(oracle.len(), cap, "push failed on a non-full ring");
                    seq -= 1; // value not enqueued; reuse it
                }
            }
        } else {
            assert_eq!(rx.pop(), oracle.pop_front(), "pop diverged from FIFO oracle");
        }
        assert_eq!(rx.len(), oracle.len(), "len diverged");
        assert_eq!(rx.is_empty(), oracle.is_empty(), "is_empty diverged");
        assert_eq!(tx.is_full(), oracle.len() == cap, "is_full diverged");
    }
    while let Some(v) = rx.pop() {
        assert_eq!(Some(v), oracle.pop_front(), "drain diverged");
    }
    assert!(oracle.is_empty(), "ring dropped queued values");

    // ---- Phase 2: two-thread stress, value conservation ----
    // Strict in-order arrival of 0..n on the consumer side is the
    // conservation proof for an SPSC FIFO. n scales with input size but
    // stays bounded so the fuzzer keeps a high exec rate.
    let cap2 = data.get(1).copied().unwrap_or(4) as usize % 64 + 1;
    let n = 64 + data.len().min(1984) as u64;
    let (mut tx, mut rx) = kevy_ring::ring::<u64>(cap2);
    if THREADED_EXECS.fetch_add(1, Ordering::Relaxed) < THREADED_EXEC_CAP {
        let producer = std::thread::spawn(move || {
            for i in 0..n {
                let mut v = i;
                loop {
                    match tx.push(v) {
                        Ok(()) => break,
                        Err(back) => {
                            v = back;
                            std::hint::spin_loop();
                        }
                    }
                }
            }
        });
        let mut next = 0u64;
        while next < n {
            match rx.pop() {
                Some(v) => {
                    assert_eq!(v, next, "SPSC lost, duplicated or reordered a value");
                    next += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
        producer.join().expect("producer thread panicked");
    } else {
        // Thread budget spent (see INFRA NOTE): same conservation check,
        // single-threaded, popping whenever the ring fills.
        let mut next = 0u64;
        for i in 0..n {
            let mut v = i;
            loop {
                match tx.push(v) {
                    Ok(()) => break,
                    Err(back) => {
                        v = back;
                        let got = rx.pop().expect("full ring must pop");
                        assert_eq!(got, next, "SPSC lost, duplicated or reordered a value");
                        next += 1;
                    }
                }
            }
        }
        while let Some(got) = rx.pop() {
            assert_eq!(got, next, "SPSC lost, duplicated or reordered a value");
            next += 1;
        }
        assert_eq!(next, n, "value count not conserved");
    }
    assert_eq!(rx.pop(), None, "ring must be empty after n in-order pops");
});
