//! The cross-shard park/wake fence, in one place so it can be model-checked.
//!
//! A shard that finds no work parks in a blocking wait. A sender that pushes
//! into a parked peer's inbox has to wake it with a syscall; a sender whose
//! peer is still spinning must not pay for one. That makes the wake
//! conditional on a flag the receiver publishes, and a conditional wake has a
//! lost-wake window:
//!
//! ```text
//!   receiver                            sender
//!   publish_parked(&parked[me])         (push landed in our outbox)
//!     parked[me].store(true, SeqCst)    fence_before_wake_scan()
//!     fence(SeqCst)                       fence(SeqCst)
//!   drain_inbound()                     peer_is_parked(&parked[dst])
//!   -> if it found nothing, block         -> if false, no wake is sent
//! ```
//!
//! Without the two fences, the receiver's store and the sender's load can
//! both be ordered after the other's, and the receiver blocks holding a
//! message nobody will wake it for. The blocking wait's `PARK_TIMEOUT_MS`
//! bounds the damage at 50 ms of latency, which is a backstop, not a fix.
//!
//! These four functions exist as functions, rather than inline at the three
//! call sites, for one reason: `tests/loom.rs` calls *these* and enumerates
//! every interleaving of them. Before that the loom suite modelled the
//! pattern with atomics it declared itself, so it proved the pattern sound
//! and said nothing about whether kevy-rt implemented it — deleting a fence
//! from production left the suite green. Now it does not.
//!
//! `ParkFlag` is a plain `AtomicBool` in every build except a `--cfg loom`
//! one, where it becomes loom's instrumented atomic and the functions below
//! become the thing loom is scheduling.

#[cfg(loom)]
pub use loom::sync::atomic::AtomicBool as ParkFlag;
#[cfg(not(loom))]
pub use std::sync::atomic::AtomicBool as ParkFlag;

#[cfg(loom)]
use loom::sync::atomic::{Ordering, fence};
#[cfg(not(loom))]
use std::sync::atomic::{Ordering, fence};

/// A fresh un-parked flag. One per shard, allocated by the runtime.
pub fn new_flag() -> ParkFlag {
    ParkFlag::new(false)
}

/// Receiver side: advertise that we are about to park, and fence so the
/// advertisement cannot be reordered after the drain attempt that follows.
///
/// The caller must drain once more *after* this and un-park via
/// [`clear_parked`] if that drain finds anything — publishing the flag and
/// then blocking without re-checking is the lost-wake bug itself.
pub fn publish_parked(flag: &ParkFlag) {
    flag.store(true, Ordering::SeqCst);
    fence(Ordering::SeqCst);
}

/// Receiver side: we are running again, so senders should stop paying for
/// wake syscalls on our behalf.
pub fn clear_parked(flag: &ParkFlag) {
    flag.store(false, Ordering::SeqCst);
}

/// Sender side: fence once before scanning the peers we pushed to.
///
/// Separate from [`peer_is_parked`] so a sender with several peers to check
/// pays for one fence and not one per peer — the pairing needs the fence
/// before the first load, not before each.
pub fn fence_before_wake_scan() {
    fence(Ordering::SeqCst);
}

/// Sender side: does this peer need a wake syscall? Only valid after
/// [`fence_before_wake_scan`] in the same scan.
pub fn peer_is_parked(flag: &ParkFlag) -> bool {
    flag.load(Ordering::SeqCst)
}
