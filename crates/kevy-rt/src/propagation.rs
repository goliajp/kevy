//! Effect-frame propagation override — how a verb whose *effect* is
//! nondeterministic (SPOP's random pick) keeps the AOF and the
//! replication stream deterministic.
//!
//! Replaying `SPOP key 3` by verb draws three *different* members on
//! every replay: an AOF restart or a replica applying the frame ends
//! up with a different remaining set than the process that answered
//! the client — silent divergence. Redis solves this by propagating
//! the effect (`SREM key <popped…>`) instead of the verb, and
//! `kevy-embedded::Store::spop` already does exactly that; this module
//! brings the server dispatch path to the same semantics.
//!
//! The verb body (which knows what it actually removed) calls
//! [`set_override`] right after mutating the store;
//! `Shard::post_write_housekeeping` — which runs immediately after
//! *every* write dispatch on the same thread — consumes it with ONE
//! take of the override shared by the AOF append and the replication
//! push, so disk and replicas always record the very same frame.
//! Because the take is unconditional and per-command, an override can
//! never leak into the next command of a pipelined batch.
//!
//! Thread-local by the same precedent as [`crate::applying_record`]:
//! a shard's store is only ever touched by its owning thread, and the
//! verb body has no other channel to the post-write hooks.

use std::cell::Cell;

/// What the post-write hooks should record for the command that just
/// executed.
///
/// ```
/// use kevy_rt::propagation::{Propagate, discard_override, set_override};
///
/// set_override(Propagate::Replace(vec![b"SREM".to_vec(), b"s".to_vec(), b"m".to_vec()]));
/// discard_override();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Propagate {
    /// Record the client's original argv unchanged (the default —
    /// every deterministic verb).
    AsIs,
    /// Record this argv instead of the client's (e.g. `SREM key
    /// <popped…>` for a non-empty SPOP).
    Replace(Vec<Vec<u8>>),
    /// Record nothing (e.g. SPOP against a missing/empty set — a no-op
    /// verb must not reach disk or replicas at all).
    Suppress,
}

thread_local! {
    /// The pending override for the command currently executing on
    /// this thread. `None` = no verb asked for one = `AsIs`.
    static OVERRIDE: Cell<Option<Propagate>> = const { Cell::new(None) };
    /// A record left for the cold path to build, set only together with a
    /// `Suppress` in [`OVERRIDE`]: a write with nowhere to be recorded
    /// never has its frames built.
    static DEFERRED: Cell<Option<kevy_verbs::Effect>> = const { Cell::new(None) };
}

/// Install a propagation override for the command currently executing.
/// Called from the verb body, after the store mutation; consumed by
/// the post-write housekeeping of that same command.
pub fn set_override(p: Propagate) {
    OVERRIDE.with(|c| c.set(Some(p)));
    DEFERRED.with(Cell::take);
}

/// Record the command currently executing by the frames its `effect`
/// describes ([`kevy_verbs::Effect::RecordId`], [`kevy_verbs::Effect::RecordClaim`]),
/// built only if the shard records the write at all: with the AOF off
/// and no replicas the frames are never made. Any other effect records
/// nothing.
///
/// ```
/// use kevy_rt::propagation::{discard_override, set_override_deferred};
/// let id = kevy_store::StreamId::new(1, 0);
/// set_override_deferred(kevy_verbs::Effect::RecordId(2, id));
/// // a dispatch site that records nothing drops it unbuilt
/// discard_override();
/// ```
pub fn set_override_deferred(effect: kevy_verbs::Effect) {
    OVERRIDE.with(|c| c.set(Some(Propagate::Suppress)));
    DEFERRED.with(|d| d.set(Some(effect)));
}

/// The effect [`set_override_deferred`] left, taken with the `Suppress`
/// that marks it.
pub(crate) fn take_deferred() -> Option<kevy_verbs::Effect> {
    DEFERRED.with(Cell::take)
}

/// Take (and clear) the pending override — [`Propagate::AsIs`] when no
/// verb set one. `Shard::post_write_housekeeping` calls this exactly
/// once per write, before both the AOF append and the replication
/// push, so the two recorders share one decision.
pub(crate) fn take_override() -> Propagate {
    OVERRIDE.with(Cell::take).unwrap_or(Propagate::AsIs)
}

/// Drop any pending override without recording anything. For dispatch
/// sites that run verb bodies *outside* the post-write-housekeeping
/// pairing — AOF replay, reshard merge, an inner Lua `redis.call` —
/// where a nondeterministic verb would otherwise leave its override
/// armed for whatever command runs next on the thread.
pub fn discard_override() {
    OVERRIDE.with(Cell::take);
    DEFERRED.with(Cell::take);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_as_is() {
        assert!(matches!(take_override(), Propagate::AsIs));
    }

    #[test]
    fn take_consumes_the_override() {
        set_override(Propagate::Replace(vec![b"SREM".to_vec(), b"k".to_vec()]));
        let Propagate::Replace(frame) = take_override() else {
            panic!("expected Replace");
        };
        assert_eq!(frame.len(), 2);
        // Second take: nothing pending — back to AsIs (no cross-command leak).
        assert!(matches!(take_override(), Propagate::AsIs));
    }

    #[test]
    fn discard_drops_without_recording() {
        set_override(Propagate::Suppress);
        discard_override();
        assert!(matches!(take_override(), Propagate::AsIs));
    }

    #[test]
    fn a_deferred_record_rides_a_suppress_and_does_not_linger() {
        let id = kevy_store::StreamId::new(1, 0);
        set_override_deferred(kevy_verbs::Effect::RecordId(2, id));
        assert!(matches!(take_override(), Propagate::Suppress));
        assert_eq!(take_deferred(), Some(kevy_verbs::Effect::RecordId(2, id)));
        set_override_deferred(kevy_verbs::Effect::RecordId(2, id));
        discard_override();
        assert_eq!(take_deferred(), None, "a discard drops it");
        set_override_deferred(kevy_verbs::Effect::RecordId(2, id));
        set_override(Propagate::Replace(vec![b"SREM".to_vec()]));
        assert_eq!(take_deferred(), None, "a later override replaces it");
    }

    #[test]
    fn last_set_wins() {
        set_override(Propagate::Suppress);
        set_override(Propagate::Replace(vec![b"SREM".to_vec()]));
        assert!(matches!(take_override(), Propagate::Replace(_)));
    }
}

use core::cell::RefCell;

thread_local! {
    /// Internal frames produced inside a shard tick (the SEGMENTED
    /// stitch): queued here because the tick runs in the commands
    /// layer, which has no AOF handle. The reactor drains the queue
    /// right after the tick and logs each frame — to the AOF only,
    /// never the replication stream: a replica runs its own window
    /// tick over its own data dir and seals its own segments.
    static TICK_FRAMES: RefCell<Vec<Vec<Vec<u8>>>> = const { RefCell::new(Vec::new()) };
}

/// Queue one internal frame for the reactor to log after this tick.
pub fn push_tick_frame(argv: Vec<Vec<u8>>) {
    TICK_FRAMES.with(|q| q.borrow_mut().push(argv));
}

/// Drain the tick's queued frames (reactor side).
pub(crate) fn take_tick_frames() -> Vec<Vec<Vec<u8>>> {
    TICK_FRAMES.with(|q| core::mem::take(&mut *q.borrow_mut()))
}
