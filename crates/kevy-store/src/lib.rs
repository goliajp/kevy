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
//! use std::borrow::Cow;
//! let mut s = Store::new();
//! s.set(b"greeting", b"hello".to_vec(), None, false, false);
//! assert_eq!(s.get(b"greeting").unwrap(), Some(Cow::Borrowed(&b"hello"[..])));
//!
//! s.hset(b"user:1", &[(b"name".to_vec(), b"alice".to_vec())]).unwrap();
//! assert_eq!(s.hget(b"user:1", b"name").unwrap(), Some(&b"alice"[..]));
//!
//! // A string command on a hash key is a type error, as in Redis.
//! assert_eq!(s.get(b"user:1"), Err(kevy_store::StoreError::WrongType));
//! ```
#![forbid(unsafe_code)]

mod accounting;
mod bio_drop;
mod bitmap;
mod clock;
mod entry;
pub mod evict;
pub mod expire;
pub use expire::ExpireStats;
pub(crate) use entry::Entry;
mod hash;
mod hash_ttl;
pub use hash_ttl::{HExpireCode, HExpireCond};
mod keyspace;
mod list;
mod scan;
mod list_ops;
mod set;
mod small_set;
pub use small_set::{SmallSetData, SmallSetIter};
mod small_hash;
pub use small_hash::{SmallHashData, SmallHashIter};
mod small_list;
pub use small_list::{SmallListData, SmallListIter};
mod small_zset;
pub use small_zset::{SmallZSetData, SmallZSetIter};
mod snapshot;
pub use snapshot::SnapshotView;
mod stream;
mod string;
mod string_rmw;
mod string_set;
mod types;
pub use types::{EvictionPolicy, RenameOutcome, StoreError};
mod util;
mod value;
mod zset;
mod zset_algebra;
mod zset_range;
pub use zset_algebra::{ZAggregate, zdiff, zinter, zintercard, zunion};
mod zset_flags;
pub use zset_flags::{ZaddFlags, ZaddReport};
pub use stream::{
    AutoclaimResult, ConsumerGroup, ConsumerState, EntryBatch, GroupCreateMode,
    LoadedGroup, LoadedPelEntry, LoadedStreamEntry, PelEntry, PendingExtended,
    PendingExtendedRow, PendingSummary, ReadGroupId, StreamData, StreamId, StreamIdError,
    XAddIdSpec, XClaimOpts, now_unix_ms, parse_explicit_id, parse_range_end,
    parse_range_start, parse_xadd_id,
};
pub use string::GetReply;
pub use util::glob_match;
pub use value::*;

pub(crate) use clock::{deadline_at, now_ns, pack_deadline, remaining_ms};
use kevy_map::KevyMap;

/// Feed kevy's monotonic clock on `wasm32-unknown-unknown`, which has no
/// `Instant`. The embedding host advances time (ns since an arbitrary fixed
/// epoch, e.g. `Date.now() * 1e6`) before TTL-sensitive ops and once per
/// reaper tick. No-op concept on native targets, where the OS clock is the
/// source — hence wasm-only.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use clock::set_clock_ns;
/// Feed kevy's wall clock (Unix-epoch millis, e.g. `Date.now()`) on
/// `wasm32-unknown-unknown`, where `SystemTime::now()` traps. Used by `XADD`
/// auto-IDs and `EXPIREAT`/`PEXPIREAT`.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use clock::set_wall_clock_ms;


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
    /// v2.4 per-field hash TTLs: key → (field → absolute unix-ms
    /// deadline). Holds ONLY keys with live field TTLs — one
    /// `is_empty()` branch per hash access when the feature is unused.
    pub(crate) hfttl: std::collections::HashMap<SmallBytes, KevyMap<SmallBytes, u64>>,
    /// Coarse cached monotonic clock (ns since [`epoch`]), refreshed by the
    /// reactor loop / reaper tick via [`Self::refresh_clock`]. Lazy expiry on
    /// the read path (`live_entry`) compares deadlines against this instead of
    /// calling `Instant::now()` per access — the Redis cached-`mstime` model.
    /// `0` (the `Default`) reads as "epoch" → keys look live until the first
    /// refresh, the safe direction (expires at most one refresh-interval late,
    /// never early — writes stamp deadlines from a *fresh* clock).
    pub(crate) cached_ns: u64,
    /// Whether lazy expiry trusts `Self::cached_ns` (set by a reactor/reaper
    /// that calls [`Self::refresh_clock`]) instead of reading a fresh clock per
    /// access. Enabled by the server reactor and the embedded background
    /// reaper; left `false` (the `Default`) for manual-reaper / bare-`Store`
    /// use, where nothing refreshes the cache so each access reads fresh —
    /// preserving "lazy expiry works without an explicit tick".
    pub(crate) cached_clock: bool,
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
    /// Count of live keys carrying a TTL — the size of Redis's "expire set"
    /// (`INFO keyspace`'s `expires=`). Maintained in O(1) at every TTL
    /// transition (`insert_entry` / `remove_entry` deltas + the in-place
    /// EXPIRE / PERSIST / SET sites) so the gauge never pays an O(n) keyspace
    /// scan; [`Self::ttl_pending_count`] is the O(n) ground truth used to
    /// assert this counter never drifts.
    pub(crate) expires: u64,
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
    /// Optional handle to the runtime's bio thread (v1.25 A.3). Set by
    /// `kevy-rt::Runtime::run` via [`Self::set_bio_drop_sender`] before
    /// the shard reactor loop starts. `None` = inline drop (bare-Store
    /// embedders, snapshots-loader programs, the test harness — anything
    /// without a kevy-rt runtime around it). Reads on the hot path are
    /// one `Option::as_ref` branch; the steady-state inline-drop path
    /// pays nothing beyond that branch.
    pub(crate) bio_drop_sender: Option<value::BioDropSender>,
    /// v1.25 A.2 batch-send buffer. Heavy `Value`s displaced by SET
    /// overwrites accumulate here instead of paying one mpsc send per
    /// drop; flushed in one `mpsc::Sender::send` at the end of every
    /// reactor iteration (via [`Self::flush_pending_drops`], invoked
    /// from `kevy-rt`'s epoll + io_uring reactor loops before the AOF
    /// fsync window). Amortising the channel cost over N drops lets
    /// the heap-heavy threshold sit at 1 KB — small enough that the
    /// Axis I 256 B – 16 KB SET tail benefits, big enough that
    /// sub-µs small-class drops still go inline (the push + flush
    /// branch would cost more than the inline free).
    ///
    /// **Latency window**: drops sit in this buffer ≤ one reactor
    /// iteration (10s of µs at busy-poll, ≤ park-timeout at idle —
    /// 50 ms by default). On a reactor with no traffic the buffer
    /// stays small (no new SETs to displace anything); on a reactor
    /// with sustained writes the per-iter flush fires fast enough
    /// that worst-case stall is bounded by `MAX_PENDING_DROPS`.
    ///
    /// **Bounded growth**: at `MAX_PENDING_DROPS` items the
    /// `maybe_offload_drop` path force-flushes — protects against
    /// pathological "thousand SETs in one iter never flush" cases
    /// (would otherwise hold thousands of Box<Value>s in RAM until
    /// the iter ends).
    pub(crate) pending_drops: Vec<Value>,
}


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

/// Heap bytes a `SmallBytes`-encoded key would own (`&[u8]` mirror of
/// `SmallBytes::heap_bytes`; 22-byte inline boundary per `kevy-bytes`).
#[inline]
pub(crate) fn key_heap_bytes_for(key: &[u8]) -> u64 {
    if key.len() <= 22 { 0 } else { key.len() as u64 }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_memory;
#[cfg(test)]
mod tests_snapshot;
#[cfg(test)]
mod tests_string_encoding;
