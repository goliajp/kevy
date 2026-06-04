//! Loom enumeration test for the cross-shard park/wake fence.
//!
//! `kevy-ring`'s SPSC ring already has its own loom suite (see
//! `crates/kevy-ring/tests/loom.rs`) covering the producer/consumer
//! handshake at the atomic level. This test sits one layer up — it
//! models the **park-bit** that `Shard::run` wraps around the ring to
//! avoid a lost-wake-up race:
//!
//! ```text
//!   Receiver side (Shard::run):
//!     ... idle loop ...
//!     parked[me].store(true, SeqCst);          // 1. advertise "I'm parking"
//!     fence(SeqCst);                           // 2. fence ↕ sender's load
//!     if drain_inbound()? {                    // 3. one more drain attempt
//!         parked[me].store(false, SeqCst);     // 4. found work → un-park
//!         continue;                            //    and process
//!     }
//!     poller.wait(PARK_TIMEOUT_MS)             // 5. block (woken by sender)
//!
//!   Sender side (send_to + flush_wakes):
//!     ring.push(msg)                           // A. push (or set a flag)
//!     fence(SeqCst);                           // B. fence ↕ receiver's store
//!     if parked[dst].load(SeqCst) {            // C. only wake if parked
//!         wakers[dst].wake()                   // D. syscall (eventfd write)
//!     }
//! ```
//!
//! The invariant: in every legal interleaving the receiver either
//! (i) sees the message in step `3` or (ii) gets a wake signal (because
//! the sender's load in `C` observed `parked=true`). The bug the test
//! guards against is "both empty": receiver drained nothing AND sender
//! skipped the wake — which would leave the receiver blocked forever
//! in production (until `PARK_TIMEOUT_MS` saved it, but that's a
//! 50 ms-latency band-aid, not a correctness fix).
//!
//! Production NOTE: the current `Shard::run` relies on the SeqCst
//! total order of `parked.store` ↔ `parked.load` alone, *without* the
//! explicit SeqCst fences modelled here. The fences are needed to
//! make the disjunction hold *without* the `PARK_TIMEOUT_MS=50 ms`
//! backstop. As written, production has a (rare) lost-wake window
//! bounded by 50 ms — acceptable for v1 but worth revisiting when
//! tail-latency tuning comes around. The loom test below documents
//! the fenced version that *would* be lock-free-correct, so a future
//! refactor can drop the timeout without regressing.
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
//! ```bash
//! RUSTFLAGS="--cfg loom" cargo test -p kevy-rt --test loom --release
//! ```
//!
//! `--release` is recommended: loom's exhaustive search explores
//! thousands of interleavings, and debug builds make each one slow.

#![allow(unexpected_cfgs)]
#![cfg(loom)]

use loom::sync::atomic::{fence, AtomicBool, Ordering};
use loom::sync::Arc;
use loom::thread;

/// Reduced model — replace the SPSC ring with one `AtomicBool` flag so
/// the test space is small enough for `LOOM_MAX_PREEMPTIONS=2`. The
/// pattern of "push payload then load parked" / "store parked then peek
/// payload" is identical; loom is exercising the SeqCst fence between
/// them, which is the actual primitive under test.
#[test]
fn park_wake_fence_no_lost_wakeup() {
    loom::model(|| {
        // Stand-in for the SPSC ring's tail-cursor: a single flag the
        // sender flips to publish one message.
        let pushed = Arc::new(AtomicBool::new(false));
        // Receiver's "is parking now?" flag (Shard.parked[me]).
        let parked = Arc::new(AtomicBool::new(false));
        // Sender → receiver wake signal. Production: eventfd/pipe write.
        let wake_signal = Arc::new(AtomicBool::new(false));

        let pushed_s = pushed.clone();
        let parked_s = parked.clone();
        let wake_s = wake_signal.clone();

        // Sender: publish + fence + load parked + maybe wake.
        let sender = thread::spawn(move || {
            // (A) Push. SeqCst store stands in for the ring's release.
            pushed_s.store(true, Ordering::SeqCst);
            // (B) Fence: any earlier SeqCst store from any thread now
            // synchronises with subsequent SeqCst loads here.
            fence(Ordering::SeqCst);
            // (C) Check if receiver is parked. If yes, send wake.
            if parked_s.load(Ordering::SeqCst) {
                // (D) Wake (in production: eventfd write).
                wake_s.store(true, Ordering::SeqCst);
            }
        });

        // Receiver: park, fence, drain once. (Phase 5 — blocking
        // poll — isn't loom-modelled; we just assert the disjunction
        // holds at the end of phase 3.)
        parked.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let drained = pushed.load(Ordering::SeqCst);
        if drained {
            parked.store(false, Ordering::SeqCst);
        }

        sender.join().unwrap();

        let got_wake = wake_signal.load(Ordering::SeqCst);
        assert!(
            drained || got_wake,
            "lost-wake under SeqCst fence model: receiver drained nothing \
             AND sender did not signal — production would block until \
             PARK_TIMEOUT_MS (50 ms) elapses, defeating the fence's purpose"
        );
    });
}

/// Same property, but flipped: assert the *contrapositive* — if the
/// sender did NOT signal a wake (sender saw parked=false), then the
/// receiver MUST have drained the published message. This is the
/// strict correctness invariant the fence buys us in addition to the
/// disjunction above.
#[test]
fn no_wake_implies_drained() {
    loom::model(|| {
        let pushed = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let wake_signal = Arc::new(AtomicBool::new(false));

        let pushed_s = pushed.clone();
        let parked_s = parked.clone();
        let wake_s = wake_signal.clone();

        let sender = thread::spawn(move || {
            pushed_s.store(true, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if parked_s.load(Ordering::SeqCst) {
                wake_s.store(true, Ordering::SeqCst);
            }
        });

        parked.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let drained = pushed.load(Ordering::SeqCst);

        sender.join().unwrap();
        let got_wake = wake_signal.load(Ordering::SeqCst);
        if !got_wake {
            assert!(
                drained,
                "fence invariant broken: sender saw parked=false (no wake) \
                 yet receiver also missed the push — both ends raced past \
                 the fence somehow"
            );
        }
    });
}
