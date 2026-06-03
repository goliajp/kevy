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

mod accounting;
pub mod evict;
pub mod expire;
pub use expire::ExpireStats;
mod hash;
mod keyspace;
mod list;
mod set;
mod string;
mod util;
mod value;
mod zset;
pub use util::glob_match;
pub use value::*;

use kevy_map::KevyMap;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Process-start anchor: every `Entry::expire_at_ns` is a nanosecond
/// offset from this `Instant`, encoded as `Option<NonZeroU64>` so the
/// niche optimisation lets the field cost 8 bytes (vs 16 for a bare
/// `Option<Instant>`). 584-year range from process start — Y2538-proof.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Encode an absolute `Instant` as ns-since-process-start. Returns `None`
/// when `t == epoch()` exactly (sentinel collision); in practice an entry
/// inserted at exactly t=0 from process start with TTL=0 is the only path
/// there, and TTL=0 isn't a valid expiry the API ever takes.
#[inline]
fn pack_deadline(t: Instant) -> Option<NonZeroU64> {
    let ns = t.saturating_duration_since(epoch()).as_nanos() as u64;
    NonZeroU64::new(ns)
}

/// Decode a packed deadline back into an `Instant` for the rare paths
/// (`pttl`, snapshot dump) that need real-clock math.
#[inline]
fn unpack_deadline(ns: NonZeroU64) -> Instant {
    epoch() + Duration::from_nanos(ns.get())
}

/// Per-entry weight ceiling — the field is `u32` so accounting saturates
/// at 4 GiB per entry. Real-world Redis values are well below this; the
/// ceiling only matters when a single hash / list / zset exceeds 4 GiB,
/// in which case `MEMORY USAGE` and the maxmemory accounting under-
/// report that one entry by the overflow amount. Acceptable v1.0 tradeoff
/// — keeps `Entry` at 48 bytes (vs 56 if we kept `u64`).
const WEIGHT_MAX: u32 = u32::MAX;

/// Per-key entry — packed to 48 bytes (vs 64 in the original
/// `Value + Option<Instant> + u64 weight + u32 clock + 4 pad` layout):
///
/// - `value`: 32 bytes (boxed-collection enum).
/// - `expire_at_ns`: `Option<NonZeroU64>` = ns since process start.
///   Niche optimisation makes this 8 bytes, not the 16 a bare
///   `Option<Instant>` would cost.
/// - `weight`: `u32`. Cached `key.heap_bytes() + value.weight()` for
///   O(1) eviction & `MEMORY USAGE`. Saturates at 4 GiB per entry.
/// - `lru_clock`: `u32`. LRU = monotonic op counter; LFU = packed
///   `[16-bit decay-tick | 8-bit log-counter]`. Only updated when
///   `Store::maxmemory > 0`.
///
/// Storage saving over the original layout: 16 bytes per entry = 25 %.
/// For a 1 M-key shard that's ~16 MB of RSS back.
pub(crate) struct Entry {
    pub(crate) value: Value,
    pub(crate) expire_at_ns: Option<NonZeroU64>,
    pub(crate) weight: u32,
    pub(crate) lru_clock: u32,
}

impl Entry {
    /// Build a fresh entry with weight + lru_clock uninitialised (the
    /// caller — usually [`Store::insert_entry`] — will compute and stamp them).
    #[inline]
    pub(crate) fn new(value: Value, expire_at: Option<Instant>) -> Self {
        Self {
            value,
            expire_at_ns: expire_at.and_then(pack_deadline),
            weight: 0,
            lru_clock: 0,
        }
    }

    /// Cached entry weight as a `u64` for arithmetic uniformity with the
    /// `Store::used_memory: u64` accumulator. Zero-cost cast.
    #[inline]
    pub(crate) fn weight(&self) -> u64 {
        self.weight as u64
    }

    /// LRU / LFU clock value (eviction-only).
    #[inline]
    pub(crate) fn lru_clock(&self) -> u32 {
        self.lru_clock
    }

    /// Overwrite the cached weight, saturating at the 4 GiB ceiling.
    #[inline]
    pub(crate) fn set_weight(&mut self, w: u64) {
        self.weight = w.min(WEIGHT_MAX as u64) as u32;
    }

    /// Overwrite the LRU/LFU clock field.
    #[inline]
    pub(crate) fn set_lru_clock(&mut self, c: u32) {
        self.lru_clock = c;
    }

    /// Apply a signed delta to the cached weight (saturating both directions).
    #[inline]
    pub(crate) fn add_to_weight(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        let cur = self.weight as u64;
        let new = if delta >= 0 {
            cur.saturating_add(delta as u64)
        } else {
            cur.saturating_sub((-delta) as u64)
        };
        self.weight = new.min(WEIGHT_MAX as u64) as u32;
    }

    /// Is the entry past its deadline as of `now`? `None` deadline =
    /// never. Combines the two-step compare into one branch on the
    /// niche-optimised `Option`.
    #[inline]
    pub(crate) fn is_expired_at(&self, now: Instant) -> bool {
        match self.expire_at_ns {
            None => false,
            Some(ns) => unpack_deadline(ns) <= now,
        }
    }
}

// Pin the Entry layout: 32 (Value) + 8 (expire_at_ns, niche-opt) + 8 (packed)
// = 48 bytes. Any padding regression (e.g. someone re-adding a 4-byte field
// without packing) is caught at compile time.
const _: () = {
    assert!(std::mem::size_of::<Entry>() == 48);
};

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
/// driver.
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
    /// Keys expired since startup (lazy reap path AND
    /// [`Self::tick_expire`]). Surfaced via `INFO keyspace` / `MEMORY STATS`
    /// once those fields land.
    pub(crate) expired_keys_total: u64,
    /// `WATCH` version counters — present only for keys that have been
    /// `WATCH`-ed at least once. [`Self::record_watch`] inserts the entry
    /// (version 0 = "never written since first watch"); every subsequent
    /// write on this shard calls [`Self::bump_if_watched`] which increments
    /// only if the key is present in the map. Keys never `WATCH`-ed pay
    /// one empty-map hashmap lookup per write (~10 ns).
    ///
    /// The map grows monotonically — entries are never evicted, even
    /// when no conn is currently watching the key. For high-key-churn
    /// workloads this can become a memory item; v1.x acceptable since
    /// the entry is `Vec<u8>` + `u64` (~ 30 B + key length) and only
    /// touched on writes / WATCH calls.
    pub(crate) watch_versions: std::collections::HashMap<Vec<u8>, u64>,
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
        *self
            .watch_versions
            .entry(key.to_vec())
            .or_insert(0)
    }

    /// Read-only version lookup used by `EXEC`'s pre-execution check.
    /// Returns `0` for keys never `WATCH`-ed (matches the initial value
    /// `record_watch` would have inserted, so a `WATCH` → no-write →
    /// `EXEC` sequence sees the stored 0 == current 0 and proceeds).
    #[inline]
    pub fn key_version(&self, key: &[u8]) -> u64 {
        self.watch_versions.get(key).copied().unwrap_or(0)
    }

    /// Bump the version of `key` if (and only if) it has been
    /// `WATCH`-ed at least once. Called from the write side of
    /// `exec_op` after every successful mutation. Cost when no key is
    /// watched: one empty-map lookup (~10 ns); when watched: lookup +
    /// in-place u64 increment.
    #[inline]
    pub fn bump_if_watched(&mut self, key: &[u8]) {
        if let Some(v) = self.watch_versions.get_mut(key) {
            *v = v.wrapping_add(1);
        }
    }

    /// Invalidate every watched key in one shot. Called from `FLUSHDB`
    /// / `FLUSHALL` execution paths — every WATCH against this shard
    /// must invalidate so a pending `EXEC` aborts.
    pub fn bump_all_watched(&mut self) {
        for v in self.watch_versions.values_mut() {
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
