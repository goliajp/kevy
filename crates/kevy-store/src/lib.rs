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
//! s.hset(b"user:1", &[(b"name".as_slice(), b"alice".as_slice())]).unwrap();
//! assert_eq!(s.hget(b"user:1", b"name").unwrap(), Some(&b"alice"[..]));
//!
//! // A string command on a hash key is a type error, as in Redis.
//! assert_eq!(s.get(b"user:1"), Err(kevy_store::StoreError::WrongType));
//! ```
#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(all(not(feature = "std"), not(feature = "external-clock")))]
compile_error!(
    "kevy-store without `std` needs the `external-clock` feature: the TTL \
     clock must be host-fed when std::time is unavailable"
);

extern crate alloc;

/// The alloc-crate slice of the std prelude, for `no_std` builds — glob-
/// imported per file so the std build stays byte-for-byte untouched.
#[cfg(not(feature = "std"))]
pub(crate) mod nostd_prelude {
    pub(crate) use alloc::boxed::Box;
    pub(crate) use alloc::format;
    pub(crate) use alloc::string::{String, ToString};
    pub(crate) use alloc::vec::Vec;
}
#[cfg(not(feature = "std"))]
use nostd_prelude::*;

/// The two side maps (`hfttl`, `watch_versions`) ride std's table on std
/// and the self-hosted `KevyMap` without it.
#[cfg(feature = "std")]
pub(crate) type SideMap<K, V> = std::collections::HashMap<K, V>;
#[cfg(not(feature = "std"))]
pub(crate) type SideMap<K, V> = kevy_map::KevyMap<K, V>;

mod accounting;
#[cfg(feature = "std")]
mod bio_drop;

/// Without `std` there is no bio thread (`bio_drop` module is compiled
/// out) — displaced heavy values drop inline on the caller.
#[cfg(not(feature = "std"))]
impl Store {
    #[inline]
    pub(crate) fn maybe_offload_drop(&mut self, old: Value) {
        drop(old);
    }
}
mod bitmap;
mod clock;
mod entry;
mod error;
pub use error::{KevyError, KevyResult};
pub mod evict;
pub mod expire;
pub use expire::ExpireStats;
pub(crate) use entry::Entry;
mod hash;
mod hash_read;
mod hash_ttl;
pub use hash_ttl::{HExpireCode, HExpireCond};
mod keyspace;
mod keyspace_load;
mod list;
pub mod list_seg;
pub mod seg_map;
mod list_read;
mod notify;
mod rng;
mod scan;
pub use notify::KeyspaceEvent;
mod list_ops;
mod set;
mod set_read;
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
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod segrows;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod segwindow;
mod tier;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod tier_codec;
mod tier_demote;
mod tier_serve;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use segrows::SealedRows;

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use segwindow::apply_segmented;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use tier::TierStats;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use tier_serve::{ColdBatchReader, ColdRead, PeekRow, SyncColdRead};
mod types;
pub use types::{EvictionPolicy, RenameOutcome, StoreError};
mod util;
mod value;
mod value_cold;
mod zset;
pub mod zset_seg;
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
pub use string::{GetReply, GetShared};
pub use util::glob_match;
pub use value::*;

pub(crate) use clock::{deadline_at, now_ns, pack_deadline, remaining_ms};
use kevy_map::KevyMap;
/// Feed kevy's monotonic clock on `wasm32-unknown-unknown`, which has no
/// `Instant`. The embedding host advances time (ns since an arbitrary fixed
/// epoch, e.g. `Date.now() * 1e6`) before TTL-sensitive ops and once per
/// reaper tick. No-op concept on native targets, where the OS clock is the
/// source — hence wasm-only.
#[cfg(any(feature = "external-clock", all(target_arch = "wasm32", target_os = "unknown")))]
pub use clock::set_clock_ns;
/// Feed kevy's wall clock (Unix-epoch millis, e.g. `Date.now()`) on
/// `wasm32-unknown-unknown`, where `SystemTime::now()` traps. Used by `XADD`
/// auto-IDs and `EXPIREAT`/`PEXPIREAT`.
#[cfg(any(feature = "external-clock", all(target_arch = "wasm32", target_os = "unknown")))]
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
    /// The random source. SPOP and SRANDMEMBER promise an ARBITRARY member;
    /// before this they returned the first one in hash-bucket order, which for
    /// a given set is the same member every time.
    pub(crate) rng: rng::Rng,
    /// Per-field hash TTLs: key → (field → absolute unix-ms
    /// deadline). Holds ONLY keys with live field TTLs — one
    /// `is_empty()` branch per hash access when the feature is unused.
    pub(crate) hfttl: SideMap<SmallBytes, KevyMap<SmallBytes, u64>>,
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
    /// Which store-origin keyspace events to capture (see
    /// [`crate::notify`]). All-off default = every hook is one byte
    /// test.
    pub(crate) notify_capture: u8,
    /// Captured events awaiting the serving layer's drain
    /// ([`Self::take_notify_events`]), in capture order.
    pub(crate) notify_events: Vec<(notify::KeyspaceEvent, Vec<u8>)>,
    /// Keys this store dropped because their TTL passed, awaiting the
    /// serving layer's drain ([`Self::take_expired_keys`]).
    ///
    /// Separate from `notify_events` and **always on**, because it
    /// carries correctness rather than observability: an expiring key
    /// must still leave every secondary index and invalidate every
    /// WATCH on it, and neither may depend on whether some client
    /// happened to subscribe to keyspace notifications.
    pub(crate) expired_keys: Vec<Vec<u8>>,
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
    pub(crate) watch_versions: SideMap<Vec<u8>, u64>,
    /// Optional handle to the runtime's bio thread. Set by
    /// `kevy-rt::Runtime::run` via [`Self::set_bio_drop_sender`] before
    /// the shard reactor loop starts. `None` = inline drop (bare-Store
    /// embedders, snapshots-loader programs, the test harness — anything
    /// without a kevy-rt runtime around it). Reads on the hot path are
    /// one `Option::as_ref` branch; the steady-state inline-drop path
    /// pays nothing beyond that branch.
    #[cfg(feature = "std")]
    pub(crate) bio_drop_sender: Option<value::BioDropSender>,
    /// Batch-send buffer. Heavy `Value`s displaced by SET
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
    #[cfg(feature = "std")]
    pub(crate) pending_drops: Vec<Value>,
    /// Transparent-tiering state. `None` = off —
    /// today's paths byte-identical ([`Store::enable_tiering`]).
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) tier: Option<tier::TierState>,
    /// Row-segment directory — the persistent second backing behind
    /// `Value::Cold` ([`segrows`]). `None` = off, today's paths
    /// byte-identical.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) segrows: Option<segrows::SegRows>,
    /// Hot gate for the stub funnels: true iff EITHER cold backing
    /// (vlog tier / row segments) is enabled. One predictable branch
    /// keeps tier_serve/tier_resolve at their no-backing cost on
    /// deployments that never demote — the measured shape of the
    /// write-path funnels (perfgate legacy angles).
    #[cfg_attr(any(not(feature = "std"), target_arch = "wasm32"), allow(dead_code))]
    pub(crate) cold_backing: bool,
    /// The promotion gate's first-touch serve scratch (`tier_serve`):
    /// a cold value decoded for ONE read, never installed in the map.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) tier_scratch: Option<Entry>,
    /// Bulk-read (no-promote peek) mode, scoped by
    /// [`Store::peek_scope`]: while set, a cold materializing read serves
    /// via pread WITHOUT setting the probation mark and WITHOUT
    /// promoting — digest / scope-move / export reads are not access
    /// signals.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) tier_peek: bool,
}


mod store_admin;

/// No row-segment backend on this target.
#[cfg(not(all(feature = "std", not(target_arch = "wasm32"))))]
impl Store {
    /// Cfg twin of the segrows accessor: always empty.
    pub fn row_seg_files(&self) -> Vec<(u32, alloc::string::String)> {
        Vec::new()
    }

    /// A v7 snapshot cannot load where the segment backend is absent.
    pub fn load_row_stub(&mut self, _key: Vec<u8>, _seq: u32, _weight: u32) {
        panic!("row-segment snapshot record on a target without the segment backend");
    }
}

// Accounting micro-helpers live in `util` (500-LOC split); re-exported
// so the crate-wide `crate::apply_delta` / `crate::key_heap_bytes_for`
// paths keep working.
pub(crate) use util::{apply_delta, key_heap_bytes_for};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_list_seg;
#[cfg(test)]
mod tests_memory;
#[cfg(test)]
mod tests_seg_map;
#[cfg(test)]
mod tests_zset_seg;
#[cfg(test)]
mod tests_snapshot;
#[cfg(test)]
mod tests_string_encoding;
#[cfg(all(test, feature = "std", not(target_arch = "wasm32")))]
mod tests_tier;
#[cfg(all(test, feature = "std", not(target_arch = "wasm32")))]
mod tests_tier_peek;
