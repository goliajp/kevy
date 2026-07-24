//! Introspection on a live [`Store`] — the embedded-mode answer to Redis's
//! `INFO` / `DBSIZE` / `TTL` / expire-set diagnostics. In-process mode has no
//! TCP endpoint to point `redis-cli` at, so these expose the same signals as
//! plain method calls on the `Store` handle.

use std::time::Duration;

use crate::store::Store;

/// Snapshot of a store's runtime counters, returned by [`Store::info`]. A
/// cheap aggregate (one mutex lock); fields mirror the individual accessors.
#[derive(Debug, Clone)]
pub struct KevyInfo {
    /// Live key count (`DBSIZE`).
    pub keys: usize,
    /// Estimated resident bytes (`INFO memory: used_memory`).
    pub used_memory: u64,
    /// Current on-disk AOF size in bytes (0 when persistence is off).
    pub aof_bytes: u64,
    /// Live keys carrying a TTL — the expire-set size. A `0` here when you
    /// expected TTLs is the tell that the TTL subsystem didn't register them.
    pub expire_pending: usize,
    /// Total keys evicted by `maxmemory` so far.
    pub evictions: u64,
    /// Total keys expired (lazy + active reaper) so far.
    pub expired_keys: u64,
    /// Tiering gauges (capacity arc T5, `# Tiering`). `None` when
    /// tiering is off — the untiered snapshot is unchanged.
    pub tiering: Option<KevyTierInfo>,
}

/// The `# Tiering` gauge set (B12) — summed across shards; field names
/// mirror the server's `INFO # Tiering` section one-to-one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KevyTierInfo {
    /// The resolved RAM budget (whole store — Σ per-shard slices).
    pub tier_budget_bytes: u64,
    /// The unified demote target (`budget·19/20 − index floor − stub
    /// floor`, saturating). **0 = the floor alone exceeds the budget.**
    pub tier_effective_target: u64,
    /// Currently-cold keys.
    pub cold_keys: u64,
    /// Σ original weights of currently-cold values.
    pub cold_bytes: u64,
    /// RAM the cold stubs cost (Σ `ENTRY_OVERHEAD + key heap bytes`).
    pub stub_bytes: u64,
    /// The index/view memory floor fed on the reaper tick.
    pub index_reserved_bytes: u64,
    /// Vlog bytes on disk.
    pub vlog_size_bytes: u64,
    /// Vlog live (non-dead) bytes.
    pub vlog_live_bytes: u64,
    /// Vlog file count.
    pub vlog_files: u64,
    /// Vlog compaction epoch (retired files).
    pub vlog_epoch: u64,
    /// Keys demoted since boot.
    pub demotions_total: u64,
    /// Keys promoted back since boot.
    pub promotions_total: u64,
}

impl Store {
    /// One-shot snapshot of the store's introspection counters. See
    /// [`KevyInfo`]. Takes the embedded mutex once; safe to call from a
    /// health endpoint.
    pub fn info(&self) -> KevyInfo {
        KevyInfo {
            keys: self.sum_shards(|i| i.store.dbsize()),
            used_memory: self.sum_shards_u64(|i| i.store.used_memory()),
            #[cfg(feature = "persist")]
            aof_bytes: self.sum_shards_u64(|i| i.aof.as_ref().map_or(0, kevy_persist::Aof::size_bytes)),
            #[cfg(not(feature = "persist"))]
            aof_bytes: 0,
            expire_pending: self.sum_shards(|i| i.store.ttl_pending_count()),
            evictions: self.sum_shards_u64(|i| i.store.evictions_total()),
            expired_keys: self.sum_shards_u64(|i| i.store.expired_keys_total()),
            tiering: self.tier_info(),
        }
    }

    /// The `# Tiering` gauges summed across shards, or `None` when
    /// tiering is off (the section is absent, not zeroed — INFO
    /// stability for untiered stores).
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub fn tier_info(&self) -> Option<KevyTierInfo> {
        self.config.tier_budget?;
        let mut t = KevyTierInfo::default();
        for shard in self.shards.iter() {
            let g = crate::store::lock_read(shard);
            let s = g.store.tier_stats();
            t.tier_budget_bytes += s.budget;
            t.tier_effective_target += s.effective_target;
            t.cold_keys += s.cold_keys;
            t.cold_bytes += s.cold_bytes;
            t.stub_bytes += s.stub_bytes;
            t.index_reserved_bytes += s.reserved_bytes;
            t.vlog_size_bytes += s.vlog_bytes;
            t.vlog_live_bytes += s.vlog_live_bytes;
            t.vlog_files += s.vlog_files;
            t.vlog_epoch += s.vlog_epoch;
            t.demotions_total += s.demotions_total;
            t.promotions_total += s.promotions_total;
        }
        Some(t)
    }

    /// No tier backend compiled in — never a section.
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    pub fn tier_info(&self) -> Option<KevyTierInfo> {
        None
    }

    /// Number of live keys that currently carry a TTL (the expire-set size,
    /// summed across shards).
    pub fn expire_pending_count(&self) -> usize {
        self.sum_shards(|i| i.store.ttl_pending_count())
    }

    /// Remaining TTL for `key` as a [`Duration`], or `None` when the key is
    /// absent or has no TTL (persistent). For the raw Redis `PTTL` sentinels
    /// (`-2` no key, `-1` no TTL) use [`Store::ttl_ms`].
    pub fn ttl(&self, key: &[u8]) -> Option<Duration> {
        let ms = self.wshard(key).store.pttl(key);
        if ms < 0 {
            None
        } else {
            Some(Duration::from_millis(ms as u64))
        }
    }
}
