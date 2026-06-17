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
}

impl Store {
    /// One-shot snapshot of the store's introspection counters. See
    /// [`KevyInfo`]. Takes the embedded mutex once; safe to call from a
    /// health endpoint.
    pub fn info(&self) -> KevyInfo {
        KevyInfo {
            keys: self.sum_shards(|i| i.store.dbsize()),
            used_memory: self.sum_shards_u64(|i| i.store.used_memory()),
            aof_bytes: self.sum_shards_u64(|i| i.aof.as_ref().map_or(0, kevy_persist::Aof::size_bytes)),
            expire_pending: self.sum_shards(|i| i.store.ttl_pending_count()),
            evictions: self.sum_shards_u64(|i| i.store.evictions_total()),
            expired_keys: self.sum_shards_u64(|i| i.store.expired_keys_total()),
        }
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
