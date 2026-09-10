//! Loom enumeration test for the cross-shard park/wake fence.
//!
//! `kevy-ring`'s SPSC ring already has its own loom suite (see
//! `crates/kevy-ring/tests/loom.rs`) covering the producer/consumer
//! handshake at the atomic level. This test sits one layer up — it
//! models the **park-bit** that `Shard::run` wraps around the ring to
//! avoid a lost-wake-up race:
//!
//! ```text
//!   Receiver side (Shard::run, uring_park):
//!     ... idle loop ...
//!     publish_parked(&parked[me]);   // 1. advertise, then fence
//!     if drain_inbound()? {          // 2. one more drain attempt
//!         clear_parked(&parked[me]); // 3. found work → un-park
//!         continue;                  //    and process
//!     }
//!     poller.wait(PARK_TIMEOUT_MS)   // 4. block (woken by sender)
//!
//!   Sender side (send_to + flush_wakes):
//!     ring.push(msg)                 // A. Release store on the tail
//!     fence_before_wake_scan();      // B. fence ↕ the receiver's
//!     if peer_is_parked(&parked[d])  // C. only wake if parked
//!         wakers[d].wake()           // D. syscall (eventfd write)
//! ```
//!
//! Those five names are `kevy_rt::park_fence`'s, and this file calls
//! them. That is the point of the module existing.
//!
//! The invariant: in every legal interleaving the receiver either
//! (i) sees the message in step `3` or (ii) gets a wake signal (because
//! the sender's load in `C` observed `parked=true`). The bug the test
//! guards against is "both empty": receiver drained nothing AND sender
//! skipped the wake — which would leave the receiver blocked forever
//! in production (until `PARK_TIMEOUT_MS` saved it, but that's a
//! 50 ms-latency band-aid, not a correctness fix).
//!
//! Two things this file used to be, and is not any more.
//!
//! Its header claimed production relied on the SeqCst total order alone,
//! *without* these fences, and therefore carried a lost-wake window
//! bounded by `PARK_TIMEOUT_MS`. Production grew all three fences some
//! time ago — `shard_run.rs` (epoll park), `uring_park.rs` (io_uring
//! park), `shard_flush.rs` (the sender). The note stayed behind because
//! nothing ran this file to notice.
//!
//! And it declared its own atomics: a faithful replica of the pattern,
//! written out a second time in the test body. It proved the pattern
//! sound and said nothing about whether kevy-rt implemented it — delete
//! the fence from all three production sites and this file stayed green,
//! while all three cited it by name as the argument for their ordering.
//! It now calls the production functions, so deleting either fence turns
//! both tests below red (checked, one fence at a time).
//!
//! ## Charter
//!
//! `loom` is a dev-only crates.io crate gated behind `--cfg loom` in
//! `Cargo.toml` — it never enters a normal `cargo build` / `cargo test`.
//! Same status as `cargo-fuzz` / `cargo-llvm-cov` (charter-exempted
//! dev-tool dep).
//!
//! ## How to run
//!
//! `python3 tools/check_loom.py` — the gate, which is what CI's `loom`
//! job runs. It demands that every `#[test]` here actually ran; before
//! it existed nothing passed `--cfg loom` and both tests below reported
//! `0 passed; ok` on every CI run. By hand:
//!
//! ```bash
//! RUSTFLAGS="--cfg loom" cargo test -p kevy-rt --test loom --release
//! ```
//!
//! `--release` is recommended: debug builds make each interleaving slow.
//! Measured, both models finish in 0.00 s at every `LOOM_MAX_PREEMPTIONS`
//! from 2 to 6 — the state space is that small.

#![allow(unexpected_cfgs)]
#![cfg(loom)]

use kevy_rt::park_fence::{self, ParkFlag};
use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread;

/// The payload's orderings are the ring's, not `SeqCst`.
///
/// `kevy-ring` publishes its tail cursor with `Release` and reads it with
/// `Acquire` (`kevy-ring/src/lib.rs`; the ring itself is that crate's own
/// loom suite's job). What arrives in a parked shard's inbox therefore
/// arrives at those orderings, and a model that published it at `SeqCst`
/// would be modelling a stronger machine than the one kevy-rt runs on.
///
/// Both fences are load-bearing under this model, verified by removing
/// each one on its own: with `publish_parked`'s fence gone, and again
/// with `fence_before_wake_scan`'s gone, both tests below fail. They pass
/// with both in place.
const PUBLISH: Ordering = Ordering::Release;
const DRAIN: Ordering = Ordering::Acquire;

/// One sender, one parking receiver, one message. Runs the three
/// production functions — `fence_before_wake_scan` / `peer_is_parked` on
/// the sender, `publish_parked` / `clear_parked` on the receiver — under
/// every interleaving loom can reach, and asserts the disjunction the
/// pairing exists to guarantee: the receiver either drained the message
/// or got a wake.
///
/// Failing it in production means a shard blocks in `Poller::wait`
/// holding a message nobody will wake it for, until `PARK_TIMEOUT_MS`
/// (50 ms) expires.
#[test]
fn park_wake_fence_no_lost_wakeup() {
    loom::model(|| {
        // Stand-in for the ring's tail cursor: one flag, published at the
        // ring's ordering. The ring itself is kevy-ring's loom suite's job.
        let pushed = Arc::new(AtomicBool::new(false));
        // The real thing: Shard.parked[me], and the real ParkFlag type.
        let parked = Arc::new(ParkFlag::new(false));
        // Stand-in for the eventfd write in `Waker::wake`.
        let woke = Arc::new(AtomicBool::new(false));

        let (pushed_s, parked_s, woke_s) = (pushed.clone(), parked.clone(), woke.clone());
        let sender = thread::spawn(move || {
            // `send_to`: the outbox push lands.
            pushed_s.store(true, PUBLISH);
            // `flush_wakes_slow`, verbatim.
            park_fence::fence_before_wake_scan();
            if park_fence::peer_is_parked(&parked_s) {
                woke_s.store(true, Ordering::SeqCst);
            }
        });

        // `Shard::run` / `uring_park`, verbatim: advertise, then drain once
        // more before blocking.
        park_fence::publish_parked(&parked);
        let drained = pushed.load(DRAIN);
        if drained {
            park_fence::clear_parked(&parked);
        }

        sender.join().unwrap();

        assert!(
            drained || woke.load(Ordering::SeqCst),
            "lost wake: the receiver drained nothing AND the sender skipped \
             the wake syscall, so this shard blocks until PARK_TIMEOUT_MS \
             (50 ms) expires with a message already in its inbox"
        );
    });
}

/// The contrapositive, which is the invariant the sender side relies on:
/// if `peer_is_parked` said no — so no wake syscall was paid for — then
/// the receiver must have seen the message on its own.
///
/// Stated this way it is the assertion that skipping the syscall is
/// *safe*, which is the whole reason the flag exists.
#[test]
fn no_wake_implies_drained() {
    loom::model(|| {
        let pushed = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(ParkFlag::new(false));
        let woke = Arc::new(AtomicBool::new(false));

        let (pushed_s, parked_s, woke_s) = (pushed.clone(), parked.clone(), woke.clone());
        let sender = thread::spawn(move || {
            pushed_s.store(true, PUBLISH);
            park_fence::fence_before_wake_scan();
            if park_fence::peer_is_parked(&parked_s) {
                woke_s.store(true, Ordering::SeqCst);
            }
        });

        park_fence::publish_parked(&parked);
        let drained = pushed.load(DRAIN);
        if drained {
            park_fence::clear_parked(&parked);
        }

        sender.join().unwrap();

        if !woke.load(Ordering::SeqCst) {
            assert!(
                drained,
                "the sender saw parked=false and skipped the wake, but the \
                 receiver missed the push too — the fence pairing did not \
                 hold and the syscall was not safe to skip"
            );
        }
    });
}
