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

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::Value;
use crate::{SmallBytes, Store, now_ns, remaining_ms};

/// A frozen, `Send` view of one store's live entries at a single instant.
pub struct SnapshotView {
    entries: Vec<(SmallBytes, Value, Option<u64>)>,
    /// Hash field TTLs frozen with the view.
    hfttl: Vec<(SmallBytes, SmallBytes, u64)>,
    /// Tiering view pinning: every vlog file that existed at
    /// collect time. A cold stub cloned into the view can only
    /// reference these, and a pinned file survives compaction until the
    /// last Arc drops — so the view's offsets stay valid for its whole
    /// life, however long the serializer thread takes.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pins: Vec<std::sync::Arc<kevy_vlog::VlogFile>>,
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

    /// Visit the frozen hash field TTLs.
    pub fn each_hash_ttl<F: FnMut(&[u8], &[u8], u64)>(&self, mut f: F) {
        for (k, field, d) in &self.hfttl {
            f(k.as_slice(), field.as_slice(), *d);
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

    /// Decode a cold stub's record against the view's pinned vlog files
    /// into a fresh hot [`Value`] — the serializer-thread read path:
    /// no store access, no promotion, memory bound = this one value.
    /// `None` when `v` is hot. A stub naming an unpinned file, a failed
    /// read, or a bad decode is a process bug (the vlog is per-boot and
    /// this process pinned every file at collect time) — surfaced
    /// loudly, never healed silently.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub fn materialize_cold(&self, v: &Value) -> Option<Value> {
        let Value::Cold(c) = v else { return None };
        let file = self
            .pins
            .iter()
            .find(|f| f.id() == c.file_id)
            .expect("tier: view stub references a file pinned at collect time");
        let (_key, payload) = file
            .read(c.vref())
            .expect("tier: pinned vlog read failed — per-boot spill file, this is a process bug");
        Some(
            crate::tier_codec::decode(c.type_tag, payload)
                .expect("tier: cold record decode failed — process bug"),
        )
    }

    /// No tier backend on this target — `Value::Cold` cannot exist.
    #[cfg(not(all(feature = "std", not(target_arch = "wasm32"))))]
    pub fn materialize_cold(&self, _v: &Value) -> Option<Value> {
        None
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
        let mut hfttl = Vec::new();
        self.hash_ttl_each(|k, f, d| {
            hfttl.push((
                crate::SmallBytes::from_slice(k),
                crate::SmallBytes::from_slice(f),
                d,
            ));
        });
        SnapshotView {
            entries,
            hfttl,
            // View pinning: capture ALL current vlog file pins with
            // the view — the frozen stubs above can only reference
            // files that exist at this instant.
            #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
            pins: self.tier_pins(),
        }
    }
}
