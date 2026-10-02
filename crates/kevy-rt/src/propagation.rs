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
//! Every setter also arms a flag that the post-write step reads on every
//! write, so an override is always taken by the write that set it and
//! can never leak into the next command of a pipelined batch.
//!
//! Thread-local by the same precedent as [`crate::applying_record`]:
//! a shard's store is only ever touched by its owning thread, and the
//! verb body has no other channel to the post-write hooks.
//!
//! ```
//! use kevy_rt::propagation::{Propagate, set_override};
//! // SPOP took `b` from `s`: the AOF and replicas get `SREM s b`, not a second draw
//! let frame = ["SREM", "s", "b"].map(|w| w.as_bytes().to_vec()).to_vec();
//! set_override(Propagate::Replace(frame));
//! # kevy_rt::propagation::discard_override();
//! ```

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
    ///
    /// ```
    /// use kevy_rt::propagation::Propagate;
    /// // what a verb records, from what it did
    /// fn record(verb: &[u8], key: &[u8], popped: &[&[u8]]) -> Propagate {
    ///     match (verb, popped) {
    ///         (b"SPOP", []) => Propagate::Suppress,
    ///         (b"SPOP", _) => {
    ///             let mut frame = vec![b"SREM".to_vec(), key.to_vec()];
    ///             frame.extend(popped.iter().map(|m| m.to_vec()));
    ///             Propagate::Replace(frame)
    ///         }
    ///         _ => Propagate::AsIs,
    ///     }
    /// }
    /// assert_eq!(record(b"SET", b"k", &[]), Propagate::AsIs);
    /// ```
    AsIs,
    /// Record this argv instead of the client's (e.g. `SREM key
    /// <popped…>` for a non-empty SPOP).
    ///
    /// ```
    /// use kevy_rt::propagation::Propagate;
    /// // what a verb records, from what it did
    /// fn record(verb: &[u8], key: &[u8], popped: &[&[u8]]) -> Propagate {
    ///     match (verb, popped) {
    ///         (b"SPOP", []) => Propagate::Suppress,
    ///         (b"SPOP", _) => {
    ///             let mut frame = vec![b"SREM".to_vec(), key.to_vec()];
    ///             frame.extend(popped.iter().map(|m| m.to_vec()));
    ///             Propagate::Replace(frame)
    ///         }
    ///         _ => Propagate::AsIs,
    ///     }
    /// }
    /// let srem = ["SREM", "s", "a"].map(|w| w.as_bytes().to_vec()).to_vec();
    /// assert_eq!(record(b"SPOP", b"s", &[b"a"]), Propagate::Replace(srem));
    /// ```
    Replace(Vec<Vec<u8>>),
    /// Record nothing (e.g. SPOP against a missing/empty set — a no-op
    /// verb must not reach disk or replicas at all).
    ///
    /// ```
    /// use kevy_rt::propagation::Propagate;
    /// // what a verb records, from what it did
    /// fn record(verb: &[u8], key: &[u8], popped: &[&[u8]]) -> Propagate {
    ///     match (verb, popped) {
    ///         (b"SPOP", []) => Propagate::Suppress,
    ///         (b"SPOP", _) => {
    ///             let mut frame = vec![b"SREM".to_vec(), key.to_vec()];
    ///             frame.extend(popped.iter().map(|m| m.to_vec()));
    ///             Propagate::Replace(frame)
    ///         }
    ///         _ => Propagate::AsIs,
    ///     }
    /// }
    /// assert_eq!(record(b"SPOP", b"empty", &[]), Propagate::Suppress);
    /// ```
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
    /// Set by every writer of [`OVERRIDE`], [`DEFERRED`] and the Lua wake
    /// buffer. A plain `bool` has no destructor, so reading it is one load
    /// with no lazy-init check; the post-write step reads only this on a
    /// deterministic non-Lua write.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// The keyspace events the command currently executing asked for in
    /// place of its default one.
    static NOTIFY: Cell<Option<Notify>> = const { Cell::new(None) };
    /// Whether this shard publishes keyspace events at all.
    static NOTIFY_WANTED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the shard running on this thread publishes keyspace events:
/// with them off, a verb body need not work out which events it would
/// ask for.
///
/// ```
/// // a thread that is no shard publishes nothing
/// assert!(!kevy_rt::propagation::notify_wanted());
/// ```
#[inline]
pub fn notify_wanted() -> bool {
    NOTIFY_WANTED.with(Cell::get)
}

pub(crate) fn set_notify_wanted(on: bool) {
    NOTIFY_WANTED.with(|c| c.set(on));
}

/// The keyspace events a write publishes when not its default — its verb,
/// lower-cased, on argument 1.
///
/// ```
/// use kevy_rt::propagation::Notify;
/// // `BLPOP a b 0` that popped from `b` is an `lpop` on `b`
/// let popped = Notify::Events(vec![(kevy_rt::NotifyKind::List, "lpop", b"b".to_vec())]);
/// assert_ne!(popped, Notify::Suppress);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notify {
    /// The write changed nothing: no event at all.
    Suppress,
    /// These events, in order, each `(class, event name, key)`.
    Events(Vec<(crate::NotifyKind, &'static str, Vec<u8>)>),
}

/// Ask for `n` in place of the executing write's default keyspace event.
/// Taken by that write's post-write step, like [`set_override`].
///
/// ```
/// use kevy_rt::propagation::{Notify, discard_override, set_notify};
/// // a pop that found nothing publishes nothing
/// set_notify(Notify::Suppress);
/// # discard_override();
/// ```
pub fn set_notify(n: Notify) {
    NOTIFY.with(|c| c.set(Some(n)));
    arm();
}

/// Take (and clear) the pending keyspace-event override.
pub(crate) fn take_notify() -> Option<Notify> {
    NOTIFY.with(Cell::take)
}

/// Note that a post-write input (override, deferred record, Lua wake key)
/// is pending for the post-write step to take.
#[inline]
pub(crate) fn arm() {
    ARMED.with(|a| a.set(true));
}

/// Whether anything was armed since the last call, clearing the flag.
#[inline]
pub(crate) fn take_armed() -> bool {
    ARMED.with(|a| a.replace(false))
}

/// Whether anything is armed, leaving the flag as it is.
#[inline]
pub(crate) fn is_armed() -> bool {
    ARMED.with(Cell::get)
}

/// Install a propagation override for the command currently executing.
/// Called from the verb body, after the store mutation; consumed by
/// the post-write housekeeping of that same command.
///
/// ```
/// use kevy_rt::propagation::{Propagate, set_override};
/// // a verb body, right after its SPOP removed nothing
/// set_override(Propagate::Suppress);
/// // a later call replaces it: the last one set is what gets recorded
/// set_override(Propagate::AsIs);
/// # kevy_rt::propagation::discard_override();
/// ```
pub fn set_override(p: Propagate) {
    OVERRIDE.with(|c| c.set(Some(p)));
    DEFERRED.with(Cell::take);
    arm();
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
    arm();
}

/// The effect [`set_override_deferred`] left, taken with the `Suppress`
/// that marks it.
pub(crate) fn take_deferred() -> Option<kevy_verbs::Effect> {
    DEFERRED.with(Cell::take)
}

/// Take (and clear) the pending override — [`Propagate::AsIs`] when no
/// verb set one. `Shard::post_write_housekeeping` calls this once per
/// write that found the armed flag set, before both the AOF append and
/// the replication push, so the two recorders share one decision.
pub(crate) fn take_override() -> Propagate {
    OVERRIDE.with(Cell::take).unwrap_or(Propagate::AsIs)
}

/// Drop any pending override without recording anything. For dispatch
/// sites that run verb bodies *outside* the post-write-housekeeping
/// pairing — AOF replay, reshard merge, an inner Lua `redis.call` —
/// where a nondeterministic verb would otherwise leave its override
/// armed for whatever command runs next on the thread.
///
/// ```
/// use kevy_rt::propagation::{Propagate, discard_override, set_override};
/// // replaying an AOF frame runs SPOP's body, which arms an override...
/// set_override(Propagate::Replace(vec![b"SREM".to_vec(), b"s".to_vec(), b"m".to_vec()]));
/// // ...that replay must not leave behind for the next command
/// discard_override();
/// ```
pub fn discard_override() {
    OVERRIDE.with(Cell::take);
    DEFERRED.with(Cell::take);
    NOTIFY.with(Cell::take);
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
    fn every_pending_input_arms_the_post_write_step() {
        let _ = take_armed();
        assert!(!take_armed(), "nothing set: the post-write step skips the takes");
        set_override(Propagate::Suppress);
        assert!(take_armed());
        assert!(!take_armed(), "the flag clears on read");
        assert!(matches!(take_override(), Propagate::Suppress));
        set_override_deferred(kevy_verbs::Effect::RecordId(2, kevy_store::StreamId::new(1, 0)));
        assert!(take_armed());
        discard_override();
        crate::push_lua_wake_key(b"q");
        assert!(take_armed());
        assert_eq!(crate::lua_wake_bridge::drain_lua_wake_buffer(), vec![b"q".to_vec()]);
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
///
/// ```
/// // a tick that sealed a segment asks for it to be logged to the AOF
/// kevy_rt::propagation::push_tick_frame(vec![b"SEGMENTED".to_vec(), b"seg-000001".to_vec()]);
/// ```
pub fn push_tick_frame(argv: Vec<Vec<u8>>) {
    TICK_FRAMES.with(|q| q.borrow_mut().push(argv));
}

/// Drain the tick's queued frames (reactor side).
pub(crate) fn take_tick_frames() -> Vec<Vec<Vec<u8>>> {
    TICK_FRAMES.with(|q| core::mem::take(&mut *q.borrow_mut()))
}
