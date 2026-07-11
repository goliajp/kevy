//! Public point-in-time snapshot view.
//!
//! [`Store::snapshot`] freezes a consistent COW view of the WHOLE
//! keyspace: all shard write locks are taken in shard order (the same
//! deterministic-order discipline as `atomic_all_shards`, so the two
//! can't deadlock against each other), every shard's O(n)-shallow
//! view is collected inside that single window, then the locks drop.
//! Writers block only for the collection (~8 ns/entry), not for the
//! caller's subsequent iteration.
//!
//! The primary consumer: rebuilding derived state after a
//! `FeedError::Resync` — freeze a view, note `changes_tail()`, scan
//! your prefix from the view, resume the feed from the noted cursor.

use kevy_store::SnapshotView;

use crate::store::{Store, lock_write};

/// A frozen, consistent point-in-time view of the whole store.
pub struct Snapshot {
    views: Vec<SnapshotView>,
}

/// One entry from [`Snapshot::each_prefix`] / [`Snapshot::keys_prefix`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// The key.
    pub key: Vec<u8>,
    /// Remaining TTL in ms at freeze time (`None` = no expiry).
    pub ttl_ms: Option<u64>,
}

impl Snapshot {
    /// Visit every entry under `prefix` as `(key, value, ttl_ms)` —
    /// the raw [`kevy_store::Value`] borrow, for callers that want the
    /// typed payload without copies.
    pub fn each_prefix<F: FnMut(&[u8], &kevy_store::Value, Option<u64>)>(
        &self,
        prefix: &[u8],
        mut f: F,
    ) {
        for view in &self.views {
            view.each(|k, v, ttl| {
                if k.starts_with(prefix) {
                    f(k, v, ttl);
                }
            });
        }
    }

    /// Collect the keys (+ TTLs) under `prefix`, unordered across
    /// shards.
    pub fn keys_prefix(&self, prefix: &[u8]) -> Vec<SnapshotEntry> {
        let mut out = Vec::new();
        self.each_prefix(prefix, |k, _, ttl| {
            out.push(SnapshotEntry { key: k.to_vec(), ttl_ms: ttl });
        });
        out
    }

    /// Total entries in the view.
    pub fn len(&self) -> usize {
        let mut n = 0;
        for view in &self.views {
            view.each(|_, _, _| n += 1);
        }
        n
    }

    /// Whether the view is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Store {
    /// Freeze a consistent point-in-time [`Snapshot`] of the whole
    /// keyspace (see the module doc for the locking discipline).
    pub fn snapshot(&self) -> Snapshot {
        // Deterministic shard order (same as atomic_all_shards) — hold
        // ALL write locks across the collection so no write lands
        // between shard freezes.
        let guards: Vec<_> = self.shards.iter().map(|s| lock_write(s)).collect();
        let views = guards.iter().map(|g| g.store.collect_snapshot()).collect();
        drop(guards);
        Snapshot { views }
    }
}
