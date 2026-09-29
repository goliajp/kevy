//! Dispatch-without-emit gate — used by the server-as-replica path
//! to apply frames pulled from an upstream primary
//! without immediately re-pushing them into this shard's own
//! `ReplicationSource`. Without the gate, a server with both an
//! upstream link AND its own primary listener (chain replication, or
//! the brief overlap during `REPLICAOF NO ONE` promotion) would emit
//! every applied frame to its own downstream replicas, double-counting
//! the offset and creating infinite chains.
//!
//! Chain replication is explicitly out of scope,
//! so the gate is defensive: it documents intent
//! and prevents the misconfig — primary + REPLICAOF together — from
//! silently corrupting the downstream offset stream.
//!
//! Usage:
//!
//! ```
//! let _g = kevy_rt::ReplicatedApplyGuard::enter();
//! // dispatch frame here — any post_write_housekeeping that hits
//! // this shard's ReplicationSource is suppressed for the duration
//! // of `_g`.
//! ```
//!
//! Scope: the gate suppresses ONLY the `ReplicationSource::push_mutation`
//! call inside [`crate::shard::Shard::post_write_housekeeping`]. AOF
//! append, WATCH version bump, keyspace notifications, and BLOCK wakes
//! all still fire — the local store state must remain correct for
//! anyone reading from this server.

use std::cell::Cell;

thread_local! {
    /// `true` while a replicated-apply scope is active on this thread.
    /// Set by [`ReplicatedApplyGuard::enter`], cleared on drop.
    static APPLYING_REPLICATED: Cell<bool> = const { Cell::new(false) };
    /// `true` while this thread applies a record other than a primary's
    /// frame: an AOF replay, a scope-move ingest.
    static REPLAYING: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is applying a record — replaying the AOF, applying
/// a frame from a primary, ingesting a moved scope — rather than serving a
/// client. An
/// internal record verb, one kevy writes and no client may send, is
/// accepted only then.
///
/// ```
/// assert!(!kevy_rt::applying_record(), "a plain thread serves clients");
/// let _g = kevy_rt::ReplicatedApplyGuard::enter();
/// assert!(kevy_rt::applying_record());
/// ```
pub fn applying_record() -> bool {
    APPLYING_REPLICATED.with(Cell::get) || REPLAYING.with(Cell::get)
}

/// RAII guard marking the current thread as applying a record — an AOF
/// replay, a scope-move ingest — for the guard's life, so the internal
/// record verbs such records carry are accepted. Nestable.
///
/// ```
/// {
///     let _g = kevy_rt::RecordApplyGuard::enter();
///     assert!(kevy_rt::applying_record());
/// }
/// assert!(!kevy_rt::applying_record());
/// ```
#[derive(Debug)]
pub struct RecordApplyGuard {
    prev: bool,
}

impl RecordApplyGuard {
    /// Enter a record-apply scope on the current thread.
    ///
    /// ```
    /// let _g = kevy_rt::RecordApplyGuard::enter();
    /// assert!(kevy_rt::applying_record());
    /// ```
    #[must_use = "RecordApplyGuard is RAII — drop it at scope end"]
    pub fn enter() -> Self {
        Self { prev: REPLAYING.with(|c| c.replace(true)) }
    }
}

impl Drop for RecordApplyGuard {
    fn drop(&mut self) {
        REPLAYING.with(|c| c.set(self.prev));
    }
}

/// RAII guard that marks the current thread as "applying a replicated
/// frame" for the guard's lifetime. The replica runner
/// enters this scope before each `dispatch` call so the apply doesn't
/// re-push the frame into this shard's own backlog.
///
/// ```
/// use kevy_rt::{Argv, ReplicatedApplyGuard, Store};
/// let mut store = Store::new();
/// let frame = Argv::from(vec![b"RPUSH".to_vec(), b"q".to_vec(), b"v".to_vec()]);
/// {
///     // this write came from upstream: apply it without feeding it back downstream
///     let _applying = ReplicatedApplyGuard::enter();
///     store.rpush(&frame[1], &[&frame[2]])?;
/// } // the scope ends and later writes replicate as usual
/// assert_eq!(store.llen(b"q")?, 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct ReplicatedApplyGuard {
    /// Prior gate value — supports nesting (caller can enter a second
    /// scope without losing the outer one's intent; drop restores).
    prev: bool,
}

impl ReplicatedApplyGuard {
    /// Enter a replicated-apply scope on the current thread. Nestable
    /// — the inner guard restores the outer state on drop.
    #[must_use = "ReplicatedApplyGuard is RAII — drop it at scope end"]
    pub fn enter() -> Self {
        let prev = APPLYING_REPLICATED.with(Cell::get);
        APPLYING_REPLICATED.with(|c| c.set(true));
        Self { prev }
    }
}

impl Drop for ReplicatedApplyGuard {
    fn drop(&mut self) {
        APPLYING_REPLICATED.with(|c| c.set(self.prev));
    }
}

/// Read the current gate value. `post_write_housekeeping` calls this
/// inside the `Some(src)` arm to decide whether to skip the
/// `push_mutation`.
pub(crate) fn is_applying_replicated() -> bool {
    APPLYING_REPLICATED.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replay_applies_records_and_ends_with_its_guard() {
        assert!(!applying_record());
        {
            let _g = RecordApplyGuard::enter();
            assert!(applying_record());
        }
        assert!(!applying_record());
    }

    #[test]
    fn default_is_off() {
        assert!(!is_applying_replicated());
    }

    #[test]
    fn guard_sets_then_clears() {
        assert!(!is_applying_replicated());
        {
            let _g = ReplicatedApplyGuard::enter();
            assert!(is_applying_replicated());
        }
        assert!(!is_applying_replicated());
    }

    #[test]
    fn guard_nests_correctly() {
        let _outer = ReplicatedApplyGuard::enter();
        assert!(is_applying_replicated());
        {
            let _inner = ReplicatedApplyGuard::enter();
            assert!(is_applying_replicated());
        }
        // Outer scope still active after inner drops.
        assert!(is_applying_replicated());
    }
}
