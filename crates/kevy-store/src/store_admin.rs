//! Store administration: the coarse cached clock, memory accounting
//! and eviction entrypoints, and the WATCH version ledger. Split from
//! `lib.rs` to keep that file under the 500-LOC house rule.

use crate::{ENTRY_OVERHEAD, EvictionPolicy, Store, StoreError, evict, now_ns};

impl Store {
    pub fn new() -> Self {
        Store::default()
    }

    /// Refresh the coarse cached clock (`Self::cached_ns`) from a single
    /// `Instant::now()`. Call once per reactor-loop batch / reaper tick; the
    /// per-access read path then skips its own clock read. Lazy expiry is
    /// coarse to this cadence (a key expires ≤ one refresh-interval late,
    /// never early — writes stamp deadlines from a fresh clock).
    #[inline]
    pub fn refresh_clock(&mut self) {
        self.cached_ns = now_ns();
    }

    /// Enable/disable trusting the cached clock for lazy expiry (see
    /// `Self::cached_ns`). Call with `true` only when something refreshes the
    /// clock regularly (the server reactor per batch, the embedded background
    /// reaper per tick); leave `false` for manual-reaper mode. Seeds the cache
    /// when enabling so the first access is accurate.
    #[inline]
    pub fn set_cached_clock(&mut self, on: bool) {
        self.cached_clock = on;
        if on {
            self.refresh_clock();
        }
    }

    /// Install (or clear, with `maxmemory == 0`) the eviction limit and
    /// policy. Cheap; safe to call repeatedly (e.g. on `CONFIG SET`).
    #[inline]
    pub fn set_max_memory(&mut self, maxmemory: u64, policy: EvictionPolicy) {
        self.maxmemory = maxmemory;
        self.eviction_policy = policy;
    }

    /// Live byte estimate (see field doc).
    #[inline]
    pub fn used_memory(&self) -> u64 {
        self.used_memory
    }

    /// `used_memory` high-water mark since startup.
    #[inline]
    pub fn used_memory_peak(&self) -> u64 {
        self.used_memory_peak
    }

    /// Configured `maxmemory` (0 = unlimited).
    #[inline]
    pub fn maxmemory(&self) -> u64 {
        self.maxmemory
    }

    /// Configured eviction policy.
    #[inline]
    pub fn eviction_policy(&self) -> EvictionPolicy {
        self.eviction_policy
    }

    /// Total keys evicted since startup.
    #[inline]
    pub fn evictions_total(&self) -> u64 {
        self.evictions_total
    }

    /// Live keys carrying a TTL (`INFO keyspace`'s `expires=`). O(1) — reads
    /// the maintained counter, not an O(n) scan (cf. [`Self::ttl_pending_count`]).
    #[inline]
    pub fn expires_count(&self) -> usize {
        self.expires as usize
    }

    /// Apply a signed delta to the [`Self::expires`] counter, clamped at 0.
    /// Centralises the saturating arithmetic for every TTL-transition site.
    #[inline]
    pub(crate) fn adjust_expires(&mut self, delta: i64) {
        if delta != 0 {
            self.expires = (self.expires as i64 + delta).max(0) as u64;
        }
    }

    /// `WATCH` — record this key in the version tracker and return its
    /// current version. Subsequent writes on this shard bump the version
    /// via [`Self::bump_if_watched`]. Caller (the conn's origin shard)
    /// stores the returned version; `EXEC` later asks every owning shard
    /// "is the version still N?" via [`Self::key_version`].
    ///
    /// Keys that have never been written stay at version 0 — the first
    /// write after a `WATCH` bumps to 1, which is what makes the "dirty"
    /// comparison work (stored 0 ≠ current 1 ⇒ abort EXEC).
    pub fn record_watch(&mut self, key: &[u8]) -> u64 {
        #[cfg(feature = "std")]
        {
            *self
                .watch_versions
                .entry(key.to_vec())
                .or_insert(0)
        }
        #[cfg(not(feature = "std"))]
        {
            // KevyMap has no entry API — insert-if-absent, then read.
            if self.watch_versions.get(key).is_none() {
                self.watch_versions.insert(key.to_vec(), 0);
            }
            self.watch_versions.get(key).copied().unwrap_or(0)
        }
    }

    /// Read-only version lookup used by `EXEC`'s pre-execution check.
    /// Returns `0` for keys never `WATCH`-ed (matches the initial value
    /// `record_watch` would have inserted, so a `WATCH` → no-write →
    /// `EXEC` sequence sees the stored 0 == current 0 and proceeds).
    #[inline]
    pub fn key_version(&self, key: &[u8]) -> u64 {
        self.watch_versions.get(key).copied().unwrap_or(0)
    }

    /// Bump the version of `key` if (and only if) it has been `WATCH`-ed at
    /// least once. Write-side call after every mutation. The empty check
    /// runs BEFORE the key is hashed — the common nothing-watched case
    /// pays one branch, not a guaranteed-miss probe.
    #[inline]
    pub fn bump_if_watched(&mut self, key: &[u8]) {
        if self.watch_versions.is_empty() {
            return;
        }
        if let Some(v) = self.watch_versions.get_mut(key) {
            *v = v.wrapping_add(1);
        }
    }

    /// Invalidate every watched key in one shot. Called from `FLUSHDB`
    /// / `FLUSHALL` execution paths — every WATCH against this shard
    /// must invalidate so a pending `EXEC` aborts.
    pub fn bump_all_watched(&mut self) {
        #[cfg(feature = "std")]
        for v in self.watch_versions.values_mut() {
            *v = v.wrapping_add(1);
        }
        #[cfg(not(feature = "std"))]
        for (_, v) in self.watch_versions.iter_mut() {
            *v = v.wrapping_add(1);
        }
    }

    /// Cached weight of `key` (dynamic part + [`ENTRY_OVERHEAD`]). Returns
    /// `None` when the key is absent or expired (no implicit reap).
    pub fn estimate_key_bytes(&self, key: &[u8]) -> Option<u64> {
        self.map.get(key).map(|e| e.weight() + ENTRY_OVERHEAD)
    }

    /// O(1) precondition check the dispatch layer calls before every write
    /// command. Returns `Err(OutOfMemory)` only when `maxmemory > 0`, the
    /// budget is already over, AND the policy is `NoEviction` (Redis
    /// behaviour). All other policies let the write proceed and recover via
    /// [`Self::try_evict_after_write`].
    #[inline]
    pub fn precheck_for_write(&self) -> Result<(), StoreError> {
        if self.maxmemory == 0 || self.used_memory <= self.maxmemory {
            return Ok(());
        }
        if self.eviction_policy == EvictionPolicy::NoEviction {
            return Err(StoreError::OutOfMemory);
        }
        Ok(())
    }

    /// Run after every write command. No-op when disabled or under budget;
    /// otherwise samples per [`Self::eviction_policy`] and removes keys until
    /// back under `maxmemory` or no eligible candidate remains. Returns the
    /// number of keys evicted (0 on the common fast path).
    #[inline]
    pub fn try_evict_after_write(&mut self) -> usize {
        if self.maxmemory == 0 || self.used_memory <= self.maxmemory {
            return 0;
        }
        evict::evict_until_under_limit(self)
    }

}
