//! Point-in-time snapshot views — the freeze half of COW serialization.
//!
//! [`Store::collect_snapshot`] walks the keyspace once and shallow-clones
//! every live entry: keys and string values copy their bytes (≤22 B inline
//! = a 24 B memcpy), collection values bump an `Arc` refcount. The pause is
//! O(n) at nanoseconds per entry — independent of collection sizes and of
//! disk speed. The returned [`SnapshotView`] is `Send`: hand it to a
//! background thread and serialize at leisure while the store keeps
//! mutating (writes copy-on-write via `Arc::make_mut`, deletions just drop
//! one strong ref — the view's data stays alive until it is dropped).
//!
//! TTLs are resolved to remaining-milliseconds at collect time, so the view
//! is a consistent instant: an entry that expires *after* the collect still
//! appears with the remaining TTL it had at that instant.

use crate::value::Value;
use crate::{SmallBytes, Store, now_ns, remaining_ms};

/// A frozen, `Send` view of one store's live entries at a single instant.
pub struct SnapshotView {
    entries: Vec<(SmallBytes, Value, Option<u64>)>,
}

// Compile-time guarantee that a view can cross to a serializer thread.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<SnapshotView>();
};

impl SnapshotView {
    /// Visit every entry as `(key, &value, ttl_ms)` — the same shape as
    /// [`Store::snapshot_each`], so serializers take either source.
    pub fn each<F: FnMut(&[u8], &Value, Option<u64>)>(&self, mut f: F) {
        for (k, v, ttl) in &self.entries {
            f(k.as_slice(), v, *ttl);
        }
    }

    /// Number of entries frozen in the view.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the view holds zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Store {
    /// Freeze a point-in-time [`SnapshotView`] of every live entry.
    ///
    /// O(n) shallow: per entry one key clone + one [`Value`] clone (string
    /// bytes copied, collections refcount-bumped) + the TTL resolved to
    /// remaining millis. Expired-but-unreaped entries are skipped, matching
    /// [`Store::snapshot_each`].
    pub fn collect_snapshot(&self) -> SnapshotView {
        let now = now_ns();
        let mut entries = Vec::with_capacity(self.map.len());
        for (k, e) in &self.map {
            if e.is_expired_at(now) {
                continue;
            }
            let ttl = e.expire_at_ns.map(|ns| remaining_ms(ns, now));
            entries.push((k.clone(), e.value.clone(), ttl));
        }
        SnapshotView { entries }
    }
}
