//! kevy-store — the keyspace.
//!
//! A single-threaded, multi-type keyspace with lazy expiration. Each Redis data
//! type is backed by a modern `std` structure — behaviour-compatible, but **not**
//! Redis's legacy encodings:
//!
//! | Type | Backing structure |
//! |------|-------------------|
//! | String | `Vec<u8>` |
//! | Hash / Set | `HashMap` / `HashSet` (hashbrown Swiss table) |
//! | List | `VecDeque` (ring buffer, O(1) ends) |
//! | Sorted set | `HashMap` + `BTreeSet<(score, member)>` (a B-tree, not a skiplist) |
//!
//! Wrong-type access returns [`StoreError::WrongType`]. The API is `&mut self`
//! and lock-free, so a thread-per-core runtime ([kevy-rt]) can own one shard per
//! core with no locking. Part of the [kevy] key–value server.
//!
//! `maxmemory` enforcement + 8 eviction policies live in [`evict`]; toggle via
//! [`Store::set_max_memory`]. With `maxmemory == 0` (the default) the hot-path
//! cost collapses to a single predicted-not-taken branch, matching the
//! "unlimited" mode in Redis byte-for-byte.
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-rt]: https://crates.io/crates/kevy-rt
//!
//! # Example
//!
//! ```
//! use kevy_store::Store;
//!
//! let mut s = Store::new();
//! s.set(b"greeting", b"hello".to_vec(), None, false, false);
//! assert_eq!(s.get(b"greeting").unwrap(), Some(&b"hello"[..]));
//!
//! s.hset(b"user:1", &[(b"name".to_vec(), b"alice".to_vec())]).unwrap();
//! assert_eq!(s.hget(b"user:1", b"name").unwrap(), Some(&b"alice"[..]));
//!
//! // A string command on a hash key is a type error, as in Redis.
//! assert_eq!(s.get(b"user:1"), Err(kevy_store::StoreError::WrongType));
//! ```
#![forbid(unsafe_code)]

pub mod evict;
mod hash;
mod list;
mod set;
mod string;
mod util;
mod value;
mod zset;
pub use util::glob_match;
pub use value::*;

use kevy_hash::KevyHash;
use kevy_map::KevyMap;
use std::time::{Duration, Instant};

/// Per-key entry. `weight` is the cached "dynamic" footprint (key heap bytes
/// + value heap bytes) used for `maxmemory` accounting — does NOT include the
/// constant per-entry slot overhead [`ENTRY_OVERHEAD`], which is added once to
/// `Store::used_memory` on insert. `lru_clock` is a 24-bit-ish access ordinal
/// (LRU = monotonic counter; LFU = packed `[16-bit decay-tick | 8-bit log-
/// counter]`) — only updated when eviction is enabled.
pub(crate) struct Entry {
    pub(crate) value: Value,
    /// Absolute monotonic deadline; `None` means no expiry.
    pub(crate) expire_at: Option<Instant>,
    /// Cached `key.heap_bytes() + value.weight()`. Kept in sync by every
    /// mutator so eviction & `MEMORY USAGE` are O(1).
    pub(crate) weight: u64,
    /// LRU ordinal / LFU packed counter. Untouched (stays 0) when
    /// `Store::maxmemory == 0`.
    pub(crate) lru_clock: u32,
}

impl Entry {
    /// Build a fresh entry with weight uninitialised (the caller — usually
    /// [`Store::insert_entry`] — will compute and stamp it).
    #[inline]
    pub(crate) fn new(value: Value, expire_at: Option<Instant>) -> Self {
        Self {
            value,
            expire_at,
            weight: 0,
            lru_clock: 0,
        }
    }
}

/// Operation errors surfaced to the command layer.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Key holds a different type than the command expects.
    WrongType,
    /// Value is not a base-10 integer (INCR family).
    NotInteger,
    /// Result would overflow `i64`.
    Overflow,
    /// Index outside the collection (LSET).
    OutOfRange,
    /// Key does not exist where the command requires one (LSET).
    NoSuchKey,
    /// Value is not a valid float (INCRBYFLOAT).
    NotFloat,
    /// `maxmemory` would be exceeded and the active eviction policy is
    /// [`EvictionPolicy::NoEviction`]. Surfaces as Redis's classic OOM error
    /// at the RESP layer.
    OutOfMemory,
}

/// Maxmemory eviction policy. Mirror of `kevy_config::EvictionPolicy` —
/// duplicated here so `kevy-store` stays a leaf crate (no `kevy-config` dep).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionPolicy {
    /// Refuse writes once `maxmemory` is hit. Default.
    #[default]
    NoEviction,
    /// Approximated LRU across all keys.
    AllKeysLru,
    /// Approximated LFU across all keys.
    AllKeysLfu,
    /// Random key across all keys.
    AllKeysRandom,
    /// Approximated LRU across keys with a TTL.
    VolatileLru,
    /// Approximated LFU across keys with a TTL.
    VolatileLfu,
    /// Random key from those with a TTL.
    VolatileRandom,
    /// Key with the shortest remaining TTL.
    VolatileTtl,
}

impl EvictionPolicy {
    /// Whether the policy ranks candidates by LRU clock (read-touches matter).
    #[inline]
    pub fn uses_lru(self) -> bool {
        matches!(self, Self::AllKeysLru | Self::VolatileLru)
    }

    /// Whether the policy ranks candidates by LFU counter (read-touches and
    /// log-counter increments matter).
    #[inline]
    pub fn uses_lfu(self) -> bool {
        matches!(self, Self::AllKeysLfu | Self::VolatileLfu)
    }

    /// Whether the policy restricts eviction to keys that carry a TTL.
    #[inline]
    pub fn is_volatile(self) -> bool {
        matches!(
            self,
            Self::VolatileLru | Self::VolatileLfu | Self::VolatileRandom | Self::VolatileTtl
        )
    }
}

/// A single-database keyspace.
///
/// The keyspace map is a [`KevyMap`] — a pure-Rust open-addressing Swiss
/// table tuned for kevy's per-shard, single-trust-domain keyspace. The
/// hasher is [`kevy_hash::KevyHash`] (one-call inlinable; no DoS hardening
/// since the shard is single-threaded with no cross-trust keys). Owning the
/// table also exposes bucket addresses for software prefetch on the batch
/// driver — see v0.metal-5 hot plan.
#[derive(Default)]
pub struct Store {
    pub(crate) map: KevyMap<SmallBytes, Entry>,
    /// Live byte estimate (dynamic per-entry weights + [`ENTRY_OVERHEAD`] per
    /// key). Compared against [`Self::maxmemory`] to drive eviction.
    pub(crate) used_memory: u64,
    /// Soft byte ceiling. `0` = unlimited; the entire accounting + eviction
    /// machinery short-circuits to a single not-taken branch in that case.
    pub(crate) maxmemory: u64,
    /// Active eviction policy. Only consulted when `used_memory > maxmemory`.
    pub(crate) eviction_policy: EvictionPolicy,
    /// Total keys evicted by [`Self::try_evict_after_write`] — surfaced via
    /// `INFO memory` / `MEMORY STATS`.
    pub(crate) evictions_total: u64,
    /// Monotonic access counter; the upper 32 bits are unused, the lower 32
    /// stamp `Entry::lru_clock` on each access while eviction is enabled.
    pub(crate) clock_counter: u64,
    /// `used_memory` peak across the shard's lifetime; surfaced as
    /// `used_memory_peak` in `INFO memory`.
    pub(crate) used_memory_peak: u64,
}

impl Store {
    pub fn new() -> Self {
        Store::default()
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

    /// Cached weight of `key` (dynamic part + [`ENTRY_OVERHEAD`]). Returns
    /// `None` when the key is absent or expired (no implicit reap).
    pub fn estimate_key_bytes(&self, key: &[u8]) -> Option<u64> {
        self.map.get(key).map(|e| e.weight + ENTRY_OVERHEAD)
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

    // ---- accounting helpers (pub(crate)) -------------------------------

    /// Insert a fresh entry, replacing any prior. Stamps `entry.weight` from
    /// the live value and key, then updates `used_memory` for either the
    /// new-key (charges [`ENTRY_OVERHEAD`]) or overwrite (weight swap) case.
    pub(crate) fn insert_entry(&mut self, key: SmallBytes, mut entry: Entry) -> Option<Entry> {
        entry.weight = key.heap_bytes() as u64 + entry.value.weight();
        if self.maxmemory > 0 {
            self.tick_clock();
            entry.lru_clock = self.clock_counter as u32;
        }
        let new_w = entry.weight;
        let prev = self.map.insert(key, entry);
        match &prev {
            Some(old) => {
                self.used_memory = self
                    .used_memory
                    .saturating_sub(old.weight)
                    .saturating_add(new_w);
            }
            None => {
                self.used_memory = self.used_memory.saturating_add(new_w + ENTRY_OVERHEAD);
            }
        }
        self.update_peak();
        prev
    }

    /// Remove a key, returning the displaced entry (`None` if absent).
    /// Frees the entry's cached weight + [`ENTRY_OVERHEAD`].
    pub(crate) fn remove_entry(&mut self, key: &[u8]) -> Option<Entry> {
        let old = self.map.remove(key)?;
        self.used_memory = self
            .used_memory
            .saturating_sub(old.weight + ENTRY_OVERHEAD);
        Some(old)
    }

    /// Apply a signed weight delta to `key`'s cached `Entry::weight` AND to
    /// the shard-wide `used_memory`. Used by in-place collection mutators
    /// (HSET adding a field, LPUSH adding an item, …) so we account in O(1)
    /// without re-walking the container.
    pub(crate) fn account_delta(&mut self, key: &[u8], delta: i64) {
        if delta == 0 {
            return;
        }
        if let Some(e) = self.map.get_mut(key) {
            apply_delta(&mut e.weight, delta);
        }
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Recompute `weight` for the entry at `key` from its current value +
    /// key, then propagate the delta to `used_memory`. Use after a wholesale
    /// in-place value swap (SET / APPEND / INCRBYFLOAT) where the prior
    /// `Value`'s weight was already cached on the entry.
    pub(crate) fn reweigh_entry(&mut self, key: &[u8]) {
        let key_heap = key_heap_bytes_for(key);
        let Some(e) = self.map.get_mut(key) else {
            return;
        };
        let new_w = key_heap + e.value.weight();
        let delta = new_w as i64 - e.weight as i64;
        e.weight = new_w;
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Advance the global access ordinal by one tick. Only invoked under
    /// `maxmemory > 0` so the wrapping_add cost stays out of the unlimited
    /// fast path.
    #[inline]
    pub(crate) fn tick_clock(&mut self) {
        self.clock_counter = self.clock_counter.wrapping_add(1);
    }

    #[inline]
    fn update_peak(&mut self) {
        if self.used_memory > self.used_memory_peak {
            self.used_memory_peak = self.used_memory;
        }
    }

    /// Hint the CPU to fetch the bucket cache line for `key` into L1. Called
    /// by the reactor's parse loop on command N+1 while command N is still
    /// being dispatched — by the time N+1 actually probes the table, the
    /// metadata line is hot. No-op when the table is empty. Cheap when not.
    #[inline]
    pub fn prefetch_for_key(&self, key: &[u8]) {
        let hash = key.kevy_hash();
        self.map.prefetch_for_hash(hash);
    }

    pub(crate) fn expired(&self, key: &[u8], now: Instant) -> bool {
        match self.map.get(key) {
            Some(e) => e.expire_at.is_some_and(|t| t <= now),
            None => false,
        }
    }

    /// Drop `key` if expired; returns whether it is live afterwards.
    pub(crate) fn reap(&mut self, key: &[u8], now: Instant) -> bool {
        if self.expired(key, now) {
            self.remove_entry(key);
            false
        } else {
            self.map.contains_key(key)
        }
    }

    /// Single-lookup lazy-expiring read: the live `Entry` for `key`, or `None` if
    /// absent or expired (expired keys are dropped here, as `reap` would).
    ///
    /// Two wins over the old `reap(now)`-then-`get` read path: (1) the clock is
    /// read **only when the entry actually carries a TTL** — most keys don't, so
    /// the common hit skips `Instant::now()` (~20–40 ns); (2) one fewer keyspace
    /// lookup on hits (was peek-expiry + `contains_key` + `get` = 3; now peek +
    /// `get` = 2). The two-phase shape (decide, then mutate/fetch) keeps the
    /// borrow checker happy without an owning key clone.
    pub(crate) fn live_entry(&mut self, key: &[u8]) -> Option<&Entry> {
        let expired = match self.map.get(key) {
            None => return None,
            Some(e) => matches!(e.expire_at, Some(t) if t <= Instant::now()),
        };
        if expired {
            self.remove_entry(key);
            return None;
        }
        if self.maxmemory > 0 {
            self.tick_clock();
            let c = self.clock_counter as u32;
            let e = self.map.get_mut(key)?;
            evict::touch_on_access(e, self.eviction_policy, c);
            return Some(&*e);
        }
        self.map.get(key)
    }

    /// Mutable [`live_entry`](Self::live_entry): the live `Entry` for `key` by
    /// `&mut`, or `None` if absent/expired (expired dropped). Same wins — clock
    /// read only on TTL'd keys, one fewer lookup than `reap`-then-`get_mut`.
    /// Read-modify commands (INCR/APPEND/…) get the entry once and mutate in
    /// place, preserving any TTL on it.
    pub(crate) fn live_entry_mut(&mut self, key: &[u8]) -> Option<&mut Entry> {
        let expired = match self.map.get(key) {
            None => return None,
            Some(e) => matches!(e.expire_at, Some(t) if t <= Instant::now()),
        };
        if expired {
            self.remove_entry(key);
            return None;
        }
        if self.maxmemory > 0 {
            self.tick_clock();
            let c = self.clock_counter as u32;
            let e = self.map.get_mut(key)?;
            evict::touch_on_access(e, self.eviction_policy, c);
            return Some(e);
        }
        self.map.get_mut(key)
    }

    // ---- generic key ops (type-agnostic) -------------------------------

    pub fn del(&mut self, keys: &[Vec<u8>]) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        for k in keys {
            if self.reap(k, now) && self.remove_entry(k.as_slice()).is_some() {
                removed += 1;
            }
        }
        removed
    }

    pub fn exists(&mut self, keys: &[Vec<u8>]) -> usize {
        keys.iter().filter(|k| self.live_entry(k).is_some()).count()
    }

    pub fn expire(&mut self, key: &[u8], ttl: Duration) -> bool {
        let now = Instant::now();
        if !self.reap(key, now) {
            return false;
        }
        if let Some(e) = self.map.get_mut(key) {
            e.expire_at = Some(now + ttl);
            true
        } else {
            false
        }
    }

    pub fn persist(&mut self, key: &[u8]) -> bool {
        let now = Instant::now();
        if !self.reap(key, now) {
            return false;
        }
        match self.map.get_mut(key) {
            Some(e) if e.expire_at.is_some() => {
                e.expire_at = None;
                true
            }
            _ => false,
        }
    }

    /// Remaining TTL in ms: `-2` no key, `-1` no expiry, else `>= 0`.
    pub fn pttl(&mut self, key: &[u8]) -> i64 {
        let now = Instant::now();
        if !self.reap(key, now) {
            return -2;
        }
        match self.map.get(key).and_then(|e| e.expire_at) {
            None => -1,
            Some(t) => t.saturating_duration_since(now).as_millis() as i64,
        }
    }

    pub fn type_of(&mut self, key: &[u8]) -> &'static str {
        let now = Instant::now();
        if !self.reap(key, now) {
            return "none";
        }
        self.map.get(key).map_or("none", |e| e.value.type_name())
    }

    pub fn dbsize(&self) -> usize {
        self.map.len()
    }

    pub fn flush(&mut self) {
        self.map.clear();
        self.used_memory = 0;
        // peak is lifetime-cumulative; intentionally not reset.
    }

    // ---- persistence hooks ---------------------------------------------

    /// Visit every live entry as `(key, &value, ttl_ms)` for snapshotting.
    pub fn snapshot_each<F: FnMut(&[u8], &Value, Option<u64>)>(&self, mut f: F) {
        let now = Instant::now();
        for (k, e) in &self.map {
            if e.expire_at.is_some_and(|t| t <= now) {
                continue;
            }
            let ttl = e
                .expire_at
                .map(|t| t.saturating_duration_since(now).as_millis() as u64);
            f(k.as_slice(), &e.value, ttl);
        }
    }

    fn insert_loaded(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        self.insert_entry(SmallBytes::from_vec(key), Entry::new(value, expire_at));
    }

    pub fn load_str(&mut self, key: Vec<u8>, value: Vec<u8>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::Str(SmallBytes::from_vec(value)), ttl_ms);
    }

    pub fn load_hash(
        &mut self,
        key: Vec<u8>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        ttl_ms: Option<u64>,
    ) {
        // Hash keys are SmallBytes; values stay Vec<u8>. From-iter converts.
        let hash_data: HashData = fields
            .into_iter()
            .map(|(f, v)| (SmallBytes::from_vec(f), v))
            .collect();
        self.insert_loaded(key, Value::Hash(Box::new(hash_data)), ttl_ms);
    }

    pub fn load_list(&mut self, key: Vec<u8>, items: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::List(Box::new(items.into_iter().collect())), ttl_ms);
    }

    pub fn load_set(&mut self, key: Vec<u8>, members: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        let set_data: SetData = members.into_iter().map(SmallBytes::from_vec).collect();
        self.insert_loaded(key, Value::Set(Box::new(set_data)), ttl_ms);
    }

    /// Collect live keys (optionally matching a glob `pattern`, up to `limit`).
    /// Used by KEYS/SCAN/RANDOMKEY. Treats expired keys as absent (no removal).
    pub fn collect_keys(&self, pattern: Option<&[u8]>, limit: Option<usize>) -> Vec<Vec<u8>> {
        let now = Instant::now();
        let mut out = Vec::new();
        for (k, e) in &self.map {
            if e.expire_at.is_some_and(|t| t <= now) {
                continue;
            }
            if let Some(p) = pattern
                && !glob_match(p, k.as_slice())
            {
                continue;
            }
            out.push(k.to_vec());
            if limit.is_some_and(|lim| out.len() >= lim) {
                break;
            }
        }
        out
    }

    pub fn load_zset(&mut self, key: Vec<u8>, pairs: Vec<(Vec<u8>, f64)>, ttl_ms: Option<u64>) {
        let mut z = ZSetData::default();
        for (m, score) in pairs {
            z.insert(&m, score);
        }
        self.insert_loaded(key, Value::ZSet(Box::new(z)), ttl_ms);
    }
}

/// Apply a signed delta to a `u64` (saturating both directions). Used by
/// `Store::account_delta` / `reweigh_entry` so the in-place mutators don't
/// have to repeat the same overflow-guarded match.
#[inline]
pub(crate) fn apply_delta(v: &mut u64, delta: i64) {
    if delta >= 0 {
        *v = v.saturating_add(delta as u64);
    } else {
        *v = v.saturating_sub((-delta) as u64);
    }
}

/// Heap bytes a `SmallBytes`-encoded key would own. Mirrors
/// `SmallBytes::heap_bytes` but takes `&[u8]` so the helper is reachable from
/// places that don't yet have the typed `SmallBytes` (e.g. `reweigh_entry`).
/// The 22-byte inline boundary is shared with the `kevy-bytes` crate.
#[inline]
pub(crate) fn key_heap_bytes_for(key: &[u8]) -> u64 {
    if key.len() <= 22 { 0 } else { key.len() as u64 }
}

#[cfg(test)]
mod tests;
