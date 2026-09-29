//! Store administration: the coarse cached clock, memory accounting
//! and eviction entrypoints, and the WATCH version ledger. Split from
//! `lib.rs` to keep that file under the 500-LOC house rule.

use crate::value::SmallBytes;
use crate::{Entry, EvictionPolicy, Store, StoreError, evict, now_ns};
use kevy_map::KevyMap;

/// A store's entries, moved out for teardown. It holds memory only — no
/// file — so it can be dropped on any thread, whenever.
///
/// ```
/// let mut store = kevy_store::Store::new();
/// let none = store.detach_entries();
/// assert!(none.is_empty());
/// ```
#[derive(Debug)]
pub struct DetachedEntries(KevyMap<SmallBytes, Entry>);

impl DetachedEntries {
    /// How many entries it holds.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Store {
    /// Move every entry out, leaving the keyspace empty, so a host closing
    /// the store can free them off its own thread. The accounting is left
    /// as it was: this is for a store on its way to being dropped.
    ///
    /// ```
    /// let mut store = kevy_store::Store::new();
    /// store.set(b"k", b"v".to_vec(), None, kevy_store::SetCondition::Always);
    /// let entries = store.detach_entries();
    /// assert_eq!(entries.len(), 1);
    /// assert_eq!(store.dbsize(), 0);
    /// std::thread::spawn(move || drop(entries)).join().unwrap();
    /// ```
    pub fn detach_entries(&mut self) -> DetachedEntries {
        // the table leaves with the entries; a table built after it is new
        self.keyspace_bytes = 0;
        DetachedEntries(self.map.detach())
    }

    /// An empty store with default settings: no maxmemory bound, no
    /// tiering budget, and no persistence attached — the caller wires
    /// those on afterwards.
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
        self.write_gate = maxmemory > 0 || self.memory_refused;
    }

    /// Refuse every growing write with [`StoreError::OutOfMemory`] (or stop
    /// refusing), whatever `maxmemory` says: the hard stop for a process
    /// that holds more memory than its tiering budget allows, which
    /// demotion cannot fix because the bytes are not in `used_memory`.
    /// Cheap; a serving layer sets it from its tick.
    ///
    /// ```
    /// use kevy_store::{Store, StoreError};
    /// let mut s = Store::new();
    /// assert!(!s.precheck_needed(), "no bound, no refusal: the write path skips the check");
    /// s.set_memory_refusal(true);
    /// assert!(s.precheck_needed() && s.memory_refused());
    /// assert_eq!(s.precheck_for_write(), Err(StoreError::OutOfMemory));
    /// s.set_memory_refusal(false);
    /// assert_eq!(s.precheck_for_write(), Ok(()));
    /// ```
    #[inline]
    pub fn set_memory_refusal(&mut self, on: bool) {
        self.memory_refused = on;
        self.write_gate = on || self.maxmemory > 0;
    }

    /// Whether growing writes are being refused (see
    /// [`Self::set_memory_refusal`]).
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert!(!s.memory_refused());
    /// s.set_memory_refusal(true);
    /// assert!(s.memory_refused());
    /// ```
    #[inline]
    pub fn memory_refused(&self) -> bool {
        self.memory_refused
    }

    /// Whether a growing write has to run [`Self::precheck_for_write`]:
    /// `maxmemory` is set, or writes are being refused. One field read, so
    /// the unbounded default costs the write path one untaken branch.
    ///
    /// ```
    /// use kevy_store::{EvictionPolicy, Store};
    /// let mut s = Store::new();
    /// assert!(!s.precheck_needed());
    /// s.set_max_memory(1 << 20, EvictionPolicy::AllKeysLru);
    /// assert!(s.precheck_needed());
    /// ```
    #[inline]
    pub fn precheck_needed(&self) -> bool {
        self.write_gate
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
            *self.watch_versions.entry(key.to_vec()).or_insert(0)
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

    /// Cached weight of `key` plus its share of the keyspace table (the
    /// table's bytes over its keys). Returns `None` when the key is absent
    /// or expired (no implicit reap).
    pub fn estimate_key_bytes(&self, key: &[u8]) -> Option<u64> {
        let share = self.map.footprint().div_ceil(self.map.len().max(1)) as u64;
        self.map.get(key).map(|e| e.weight() + share)
    }

    /// O(1) precondition check the dispatch layer calls before every write
    /// command. Returns `Err(OutOfMemory)` when writes are being refused
    /// ([`Self::set_memory_refusal`]), or when `maxmemory > 0`, the
    /// budget is already over, AND the policy is `NoEviction` (Redis
    /// behaviour). All other policies let the write proceed and recover via
    /// [`Self::try_evict_after_write`].
    #[inline]
    pub fn precheck_for_write(&self) -> Result<(), StoreError> {
        if self.memory_refused {
            return Err(StoreError::OutOfMemory);
        }
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
