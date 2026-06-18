//! Dispatch-without-emit gate — used by the server-as-replica path
//! (Phase 1.F, T1.29) to apply frames pulled from an upstream primary
//! without immediately re-pushing them into this shard's own
//! `ReplicationSource`. Without the gate, a server with both an
//! upstream link AND its own primary listener (chain replication, or
//! the brief overlap during `REPLICAOF NO ONE` promotion) would emit
//! every applied frame to its own downstream replicas, double-counting
//! the offset and creating infinite chains.
//!
//! v1.18 explicitly forbids chain replication (see Anti-scope in the
//! v3-cluster plan), so the gate is defensive: it documents intent
//! and prevents the misconfig — primary + REPLICAOF together — from
//! silently corrupting the downstream offset stream.
//!
//! Usage:
//!
//! ```ignore
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
}

/// RAII guard that marks the current thread as "applying a replicated
/// frame" for the guard's lifetime. The replica runner (T1.29(b))
/// enters this scope before each `dispatch` call so the apply doesn't
/// re-push the frame into this shard's own backlog.
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
