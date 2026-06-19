//! Per-scope migration state machine for the Q3=(a) quiesce-window
//! `MOVE-SCOPE` design (RFC `## Q3 resolution`). Tracks two
//! concurrent maps:
//!
//! - **MIGRATING** — the writer (`from`) has stopped accepting
//!   writes for the prefix and is shipping its slice to the target
//!   (`to`). Writes during this window answer `-QUIESCED <prefix>
//!   migrating to <host:port>`.
//! - **MIGRATED** — the writer (`from`) has committed the slice
//!   to `to`. Writes here now answer `-MISDIRECTED writer is
//!   <to-host:port>` (no quiesce — the migration is done; the
//!   client should follow). The MIGRATED table is per-node local
//!   state; other nodes pick up the new writer via static config
//!   restart (v1.21 MVP — no gossip, per the anti-scope).
//!
//! This module is pure data + a single `Mutex` under the hood.
//! Server cement layer plugs the start / commit / abort transitions
//! into the `MOVE-SCOPE` / `MOVE-SCOPE-INGEST` command handlers.

use std::collections::HashMap;
use std::sync::Mutex;

/// One in-flight migration. Carries enough metadata so the server
/// cement can encode `-QUIESCED <prefix> migrating to <host:port>`
/// without re-resolving the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationState {
    /// Source writer node id (the node currently quiescing).
    pub from: String,
    /// Target writer node id (the node receiving the slice).
    pub to: String,
}

/// Per-scope migration tracker. Insert order doesn't matter; lookups
/// are O(prefix-set-size), expected ≤ tens of entries even in the
/// largest cluster.
#[derive(Debug, Default)]
pub struct MigrationTable {
    migrating: Mutex<HashMap<Vec<u8>, MigrationState>>,
    migrated: Mutex<HashMap<Vec<u8>, MigrationState>>,
}

/// Why [`MigrationTable::start`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationError {
    /// A migration for this prefix is already in flight (idempotent
    /// retry would clobber the state).
    AlreadyMigrating,
    /// A prior migration's commit hasn't been observed locally yet.
    /// Caller should abort the prior one first or accept the new
    /// state as a no-op.
    AlreadyMigrated,
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyMigrating => write!(f, "migration for this prefix is already in flight"),
            Self::AlreadyMigrated => write!(f, "prefix has already been migrated"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl MigrationTable {
    /// Empty table — the v1.21 default (no migrations in flight).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a migration for `prefix`. Returns `Err` if the prefix
    /// is already in either map. The caller (server cement) is
    /// expected to hold a higher-level lock per `MOVE-SCOPE`
    /// invocation so concurrent operator commands don't race.
    pub fn start(
        &self,
        prefix: Vec<u8>,
        from: String,
        to: String,
    ) -> Result<(), MigrationError> {
        let mut mig = self
            .migrating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if mig.contains_key(&prefix) {
            return Err(MigrationError::AlreadyMigrating);
        }
        let done = self
            .migrated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if done.contains_key(&prefix) {
            return Err(MigrationError::AlreadyMigrated);
        }
        drop(done);
        mig.insert(prefix, MigrationState { from, to });
        Ok(())
    }

    /// Commit an in-flight migration: move the entry from MIGRATING
    /// to MIGRATED. Returns the state that was committed, or `None`
    /// when there's no in-flight migration for `prefix` (idempotent).
    pub fn commit(&self, prefix: &[u8]) -> Option<MigrationState> {
        let mut mig = self
            .migrating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = mig.remove(prefix)?;
        drop(mig);
        let mut done = self
            .migrated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        done.insert(prefix.to_vec(), entry.clone());
        Some(entry)
    }

    /// Abort an in-flight migration without committing. Drops the
    /// entry from MIGRATING; the MIGRATED map is untouched (an
    /// aborted migration never moved data). Returns the state that
    /// was aborted, or `None` when there was nothing to abort.
    pub fn abort(&self, prefix: &[u8]) -> Option<MigrationState> {
        let mut mig = self
            .migrating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mig.remove(prefix)
    }

    /// Look up the in-flight migration for `prefix`. Returns the
    /// state by value (cheap clone — two short Strings) so the
    /// caller doesn't hold the mutex while encoding the
    /// `-QUIESCED` reply.
    #[must_use]
    pub fn lookup_migrating(&self, prefix: &[u8]) -> Option<MigrationState> {
        self.migrating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(prefix)
            .cloned()
    }

    /// Same shape as [`Self::lookup_migrating`], but for committed
    /// migrations (post-MOVE-SCOPE-INGEST).
    #[must_use]
    pub fn lookup_migrated(&self, prefix: &[u8]) -> Option<MigrationState> {
        self.migrated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(prefix)
            .cloned()
    }

    /// Longest-prefix-match lookup for in-flight migrations. Used by
    /// `scope_integration::route_write` to decide whether to encode
    /// `-QUIESCED` before falling through to the static
    /// `OwnershipTable::route`.
    #[must_use]
    pub fn match_migrating(&self, key: &[u8]) -> Option<MigrationState> {
        let g = self
            .migrating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The expected prefix count is small (operator-declared per-
        // cluster); a linear scan is fine. Choose the longest match
        // so nested prefixes route deterministically.
        let mut best: Option<(&Vec<u8>, &MigrationState)> = None;
        for (p, st) in g.iter() {
            if key.starts_with(p)
                && best.is_none_or(|(prev, _)| p.len() > prev.len())
            {
                best = Some((p, st));
            }
        }
        best.map(|(_, st)| st.clone())
    }

    /// As [`Self::match_migrating`], for the MIGRATED map.
    #[must_use]
    pub fn match_migrated(&self, key: &[u8]) -> Option<MigrationState> {
        let g = self
            .migrated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut best: Option<(&Vec<u8>, &MigrationState)> = None;
        for (p, st) in g.iter() {
            if key.starts_with(p)
                && best.is_none_or(|(prev, _)| p.len() > prev.len())
            {
                best = Some((p, st));
            }
        }
        best.map(|(_, st)| st.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_then_lookup() {
        let t = MigrationTable::new();
        t.start(b"app:billing:".to_vec(), "A".into(), "B".into())
            .unwrap();
        let st = t.lookup_migrating(b"app:billing:").unwrap();
        assert_eq!(st.from, "A");
        assert_eq!(st.to, "B");
    }

    #[test]
    fn start_double_errs_already_migrating() {
        let t = MigrationTable::new();
        t.start(b"p:".to_vec(), "A".into(), "B".into()).unwrap();
        let err = t.start(b"p:".to_vec(), "A".into(), "B".into()).unwrap_err();
        assert_eq!(err, MigrationError::AlreadyMigrating);
    }

    #[test]
    fn commit_moves_to_migrated() {
        let t = MigrationTable::new();
        t.start(b"p:".to_vec(), "A".into(), "B".into()).unwrap();
        let committed = t.commit(b"p:").unwrap();
        assert_eq!(committed.to, "B");
        assert!(t.lookup_migrating(b"p:").is_none());
        assert_eq!(t.lookup_migrated(b"p:").map(|s| s.to), Some("B".into()));
    }

    #[test]
    fn abort_drops_migrating() {
        let t = MigrationTable::new();
        t.start(b"p:".to_vec(), "A".into(), "B".into()).unwrap();
        t.abort(b"p:").unwrap();
        assert!(t.lookup_migrating(b"p:").is_none());
        assert!(t.lookup_migrated(b"p:").is_none());
    }

    #[test]
    fn match_migrating_longest_prefix_wins() {
        let t = MigrationTable::new();
        t.start(b"app:".to_vec(), "A".into(), "B".into()).unwrap();
        t.start(b"app:billing:".to_vec(), "B".into(), "C".into())
            .unwrap();
        // Note: kevy-scope's OwnershipTable rejects overlapping
        // prefixes at startup, but the MigrationTable is operator-
        // driven runtime state; it has to handle whatever the
        // operator types. Longest match keeps the routing
        // deterministic.
        let st = t.match_migrating(b"app:billing:x").unwrap();
        assert_eq!(st.from, "B"); // longest prefix wins
    }
}
