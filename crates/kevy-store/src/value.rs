//! Value types — one backing structure per Redis type.

pub use kevy_bytes::SmallBytes;
use kevy_map::{KevyMap, KevySet};
use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

/// Backing structure for a Hash value — [`KevyMap`] keyed by [`SmallBytes`]
/// (22 B inline / heap-else). Field names ≤22B (the vast majority — `name`,
/// `email`, etc.) live entirely inside the bucket, saving the 24 B Vec
/// metadata + heap allocation per field on a 22-byte budget.
pub type HashData = KevyMap<SmallBytes, Vec<u8>>;
/// Backing structure for a List value (a ring-buffer deque — O(1) both ends).
pub type ListData = VecDeque<Vec<u8>>;
/// Backing structure for a Set value — [`KevySet`] of [`SmallBytes`].
pub type SetData = KevySet<SmallBytes>;

/// A total-ordered f64 score (Redis scores are never NaN). `total_cmp` gives a
/// total order so scores can key a `BTreeSet`.
#[derive(Clone, Copy, PartialEq)]
pub struct Score(pub f64);
impl Eq for Score {}
impl Ord for Score {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A score-range endpoint for `ZRANGEBYSCORE`/`ZCOUNT` (inclusive or exclusive).
/// Use `value = ±INFINITY` for `-inf`/`+inf`.
pub struct ScoreBound {
    pub value: f64,
    pub exclusive: bool,
}
impl ScoreBound {
    /// Does `s` satisfy this as a *minimum* bound?
    pub(crate) fn ge_ok(&self, s: f64) -> bool {
        if self.exclusive {
            s > self.value
        } else {
            s >= self.value
        }
    }
    /// Does `s` satisfy this as a *maximum* bound?
    pub(crate) fn le_ok(&self, s: f64) -> bool {
        if self.exclusive {
            s < self.value
        } else {
            s <= self.value
        }
    }
}

/// Sorted set: a member→score map plus a B-tree ordered by `(score, member)`.
/// (A B-tree is cache-friendlier than Redis's skiplist; `ZRANK` is O(n) here —
/// an order-statistics tree for O(log n) rank is a later perf item.)
#[derive(Default, Clone)]
pub struct ZSetData {
    pub(crate) by_member: KevyMap<SmallBytes, f64>,
    /// The `(score, member)` index is still keyed by `Vec<u8>` member —
    /// changing this requires the `BTreeSet` to accept a `(Score,
    /// SmallBytes)` ordering, which is fine but a larger sweep; keep
    /// it as-is for now to avoid touching ZRANGE paths.
    pub(crate) by_score: BTreeSet<(Score, Vec<u8>)>,
}

impl ZSetData {
    pub(crate) fn insert(&mut self, member: &[u8], score: f64) -> bool {
        let is_new = match self.by_member.insert(SmallBytes::from_slice(member), score) {
            Some(old) => {
                self.by_score.remove(&(Score(old), member.to_vec()));
                false
            }
            None => true,
        };
        self.by_score.insert((Score(score), member.to_vec()));
        is_new
    }
    pub(crate) fn remove(&mut self, member: &[u8]) -> bool {
        match self.by_member.remove(member) {
            Some(old) => {
                self.by_score.remove(&(Score(old), member.to_vec()));
                true
            }
            None => false,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.by_member.len()
    }
    /// `(member, score)` pairs in ascending `(score, member)` order.
    pub fn ordered(&self) -> impl Iterator<Item = (&[u8], f64)> {
        self.by_score.iter().map(|(s, m)| (m.as_slice(), s.0))
    }
}

/// A stored value. One variant per Redis type.
///
/// The collection variants live behind a **shared pointer** (`Arc`) so the
/// enum is only as big as `Str` (24 B) + tag = 32 B, not the 56 B `ZSetData`
/// — every `Entry` (incl. the common string case) is then ~48 B instead of
/// ~80 B, so the bucket array is ~40% denser/smaller (fewer cache misses on
/// a large keyspace, less RSS). The extra pointer-chase lands only on
/// collection ops, not the hot string GET path.
///
/// `Arc` (same 8 B as the previous `Box`) is what makes O(short-pause)
/// persistence possible: [`crate::Store::collect_snapshot`] bumps each
/// collection's refcount instead of serializing it, and a background thread
/// walks the frozen payloads at leisure. Mutations go through
/// [`std::sync::Arc::make_mut`] — a single uniqueness check (the steady
/// state, no snapshot in flight) or a copy-on-write deep clone when a
/// snapshot still holds the payload.
///
/// `Str` holds a [`SmallBytes`] (24 B, same size as `Vec<u8>`) so byte strings
/// up to 22 bytes live **inline inside the bucket**, killing the second cache
/// miss the value pointer-chase used to cost on large-keyspace GETs.
/// `Clone` is the snapshot-collect primitive: `Str` copies its bytes
/// (inline = 24 B memcpy; heap = one allocation), collections bump a
/// refcount. See [`crate::Store::collect_snapshot`].
#[derive(Clone)]
pub enum Value {
    Str(SmallBytes),
    /// L2 (2026-06-21, lessons from valkey OBJ_ENCODING_INT): when a SET
    /// stores a clean canonical i64 ASCII string (parses round-trip), we
    /// keep the integer **as i64** rather than as 22 B of inline bytes.
    /// Wins on INCR (in-place `+= delta`, no parse / no format / no
    /// SmallBytes wrap) and on memory (8 B vs 24 B). GET formats it via
    /// a per-`Store` scratch buffer.
    Int(i64),
    /// L1 (2026-06-21): values larger than [`BULK_THRESHOLD`] bytes get
    /// stored behind an `Arc<[u8]>` instead of a heap-backed
    /// `SmallBytes`. The Arc lets the io_uring reactor's reply path
    /// borrow the bytes across the SQE→CQE window safely (Arc clone
    /// keeps them alive even if the keyspace mutates) — the prerequisite
    /// for the writev zero-copy bulk reply path, which skips the per-GET
    /// memcpy from value storage into the per-conn output buffer. Small
    /// values stay on `Str(SmallBytes)` because the inline cache-line
    /// storage beats an Arc indirection for the common case.
    ArcBulk(Arc<[u8]>),
    Hash(Arc<HashData>),
    List(Arc<ListData>),
    Set(Arc<SetData>),
    ZSet(Arc<ZSetData>),
    Stream(Arc<crate::stream::StreamData>),
}

/// Threshold (bytes) above which a SET stores its value as
/// [`Value::ArcBulk`] (writev-eligible on GET) instead of [`Value::Str`]
/// (inline `SmallBytes`). 64 B ≈ one cache line — below that the
/// inline-SmallBytes storage wins on L1 locality; above it the
/// writev-borrow win dominates.
pub const BULK_THRESHOLD: usize = 64;

const _: () = {
    // Don't let future variants undo box-collection's Entry-48B win.
    assert!(std::mem::size_of::<Value>() <= 32);
};

/// Heap-size threshold above which an overwritten `Value` is sent to the
/// runtime's bio thread for off-reactor drop instead of being freed inline
/// (v1.25 A.3 lazy-drop).
///
/// **Calibrated 2026-06-22 from the bench-with-256-B-floor R3 ★ finding**:
/// dropping the threshold to 256 B regressed Axis I c=50 -d 10240 SET
/// p999 from 0.487 → 1.583 ms (worse by 3.25×). The cause: `std::sync::mpsc::Sender::send`
/// is a few hundred ns of atomic + Box clone, which EXCEEDS the inline
/// `Box::<[u8]>::drop` cost when jemalloc serves the free from a hot
/// large-class slab (~ 1-3 µs for 10 KB; the bench's steady state).
/// Off-loading only wins when the inline drop's tail risk (cold-slab
/// `munmap`/`madvise` consolidation stall, observed at 50-150 µs and
/// occasionally millisecond-range) exceeds the per-send channel cost
/// PLUS the cross-thread cache-line bouncing.
///
/// 16 KB is jemalloc's `huge`/`large` class boundary on glibc Linux —
/// allocations at or above this size are far more likely to backed by
/// `mmap` directly and freed via `munmap` (the stall mode). 10 KB
/// allocations (the Axis I workload) stay on inline-drop because at
/// median they're fast; the bio path catches the genuinely huge
/// (≥ 16 KB) cases where the stall risk dominates.
///
/// Future work (per `bench/V125-OPEN-ITEMS.md` follow-up): a
/// drain-on-batch model — accumulate `Box<Value>` into a per-shard
/// `Vec` and send the WHOLE batch over one mpsc message at flush
/// time. Amortises the channel cost across N drops, restoring the
/// 256-B floor as profitable. Out of scope for the bio-thread
/// infrastructure landing.
pub const HEAP_HEAVY_BYTES: usize = 16 * 1024;

/// Sender half of the runtime's bio-drop channel. Wired from
/// `kevy-rt`'s `bio.rs` via [`crate::Store::set_bio_drop_sender`]; the
/// concrete payload is `Box<Value>` so the store crate can ship the
/// type without a back-dep on the rt crate's enum. The bio thread
/// (`kevy-rt::bio::spawn`) just runs `drop(boxed_value)`.
pub type BioDropSender = std::sync::mpsc::Sender<Box<Value>>;

impl Value {
    /// The Redis type name (`TYPE` command).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) | Value::Int(_) | Value::ArcBulk(_) => "string",
            Value::Hash(_) => "hash",
            Value::List(_) => "list",
            Value::Set(_) => "set",
            Value::ZSet(_) => "zset",
            Value::Stream(_) => "stream",
        }
    }

    /// Approximate heap bytes the value owns. Excludes the inline `Entry` /
    /// bucket slot — that's a separate per-entry constant accounted by the
    /// store. Walks collections, so prefer the cached `Entry::weight` for
    /// hot-path accounting and only call this when bootstrapping or after a
    /// load-from-snapshot.
    pub fn weight(&self) -> u64 {
        match self {
            Value::Str(s) => s.heap_bytes() as u64,
            // i64 fits in the enum tag's space; no heap.
            Value::Int(_) => 0,
            // Arc<[u8]> heap = the byte slice itself (refcount overhead
            // is amortised across shared clones).
            Value::ArcBulk(a) => a.len() as u64,
            Value::Hash(h) => collection_overhead(h.capacity(), HASH_SLOT_BYTES) + h
                .iter()
                .map(|(f, v)| f.heap_bytes() as u64 + v.capacity() as u64)
                .sum::<u64>(),
            Value::List(l) => (l.capacity() as u64).saturating_mul(LIST_SLOT_BYTES)
                + l.iter().map(|v| v.capacity() as u64).sum::<u64>(),
            Value::Set(s) => collection_overhead(s.capacity(), SET_SLOT_BYTES) + s
                .iter()
                .map(|m| m.heap_bytes() as u64)
                .sum::<u64>(),
            Value::ZSet(z) => collection_overhead(z.by_member.capacity(), HASH_SLOT_BYTES)
                + z.by_member
                    .iter()
                    .map(|(m, _)| m.heap_bytes() as u64)
                    .sum::<u64>()
                + (z.by_score.len() as u64).saturating_mul(BTREE_SLOT_BYTES),
            Value::Stream(s) => s.weight(),
        }
    }

    /// Whether this value's `Drop` is heavy enough to deserve being
    /// shipped to the bio thread instead of freed inline. Fast: every
    /// variant decides off a sub-field cheap to inspect (no recursive
    /// walk), so it's safe to call on every overwrite-SET on the hot
    /// path. The threshold is intentionally conservative — small Arcs
    /// + every short string stay on inline-drop where jemalloc small-
    /// class is sub-µs and a cross-thread hand-off would lose.
    #[inline]
    pub fn is_heap_heavy(&self) -> bool {
        match self {
            // Inline 22 B / heap ≤ small-class — fast to free inline.
            Value::Str(_) | Value::Int(_) => false,
            // The Axis I culprit. v1.25 A.3 lazy-drop's primary case.
            Value::ArcBulk(a) => a.len() >= HEAP_HEAVY_BYTES,
            // Collection drops walk every element + the bucket array;
            // worst-case microseconds on a multi-KB hash/zset. Send to
            // bio so a SET that overwrites a collection-typed key (the
            // Redis polymorphic case) doesn't stall the reactor.
            //
            // The check uses `Arc::strong_count == 1` to avoid sending
            // a still-shared Arc: another holder (a SnapshotView in
            // flight, a same-shard live read) would force the bio
            // thread to only do a refcount-decrement, which is wasted
            // cross-thread traffic. A unique Arc IS the case where
            // drop is expensive (it really frees the inner payload).
            Value::Hash(a) => std::sync::Arc::strong_count(a) == 1 && !a.is_empty(),
            Value::List(a) => std::sync::Arc::strong_count(a) == 1 && !a.is_empty(),
            Value::Set(a) => std::sync::Arc::strong_count(a) == 1 && !a.is_empty(),
            Value::ZSet(a) => {
                std::sync::Arc::strong_count(a) == 1 && a.by_member.len() > 0
            }
            Value::Stream(a) => {
                std::sync::Arc::strong_count(a) == 1 && a.length() > 0
            }
        }
    }
}

// `BioDropSender = mpsc::Sender<Box<Value>>` requires `Value: Send`. Static
// assert: if a future variant inadvertently makes Value `!Send` (e.g. an
// `Rc<...>` payload) this fails at compile time, BEFORE the runtime tries
// to hand a value to the bio thread.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Value>();
};

/// Per-bucket footprint for `KevyMap`/`KevySet`-backed collections (open-
/// addressing Swiss table). Approximation, not exact: includes metadata byte
/// per slot plus the boxed `K`/`V` cell, padded for 7/8 load factor.
pub(crate) const HASH_SLOT_BYTES: u64 = 32;
pub(crate) const SET_SLOT_BYTES: u64 = 24;
/// `VecDeque` ring-buffer slot per stored `Vec<u8>` header (24 B Vec metadata).
pub(crate) const LIST_SLOT_BYTES: u64 = 24;
/// `BTreeSet` per-entry overhead (node pointers + 6-element B-tree node padding).
pub(crate) const BTREE_SLOT_BYTES: u64 = 40;
/// Per-entry overhead in the top-level keyspace map: the inline 24-byte
/// `SmallBytes` key cell + the 64-byte `Entry` (post weight/clock fields) +
/// metadata. Approximation that errs slightly high so `used_memory` stays a
/// conservative upper bound vs the actual allocator footprint.
pub const ENTRY_OVERHEAD: u64 = 96;

#[inline]
fn collection_overhead(capacity: usize, per_slot: u64) -> u64 {
    (capacity as u64).saturating_mul(per_slot)
}

/// Per-field delta a new hash field charges against the entry weight: heap
/// bytes for the field name (if not inline) + value capacity + one slot of
/// bucket overhead. Used when an HSET inserts a brand-new field.
#[inline]
pub fn hash_field_weight(field: &SmallBytes, value_cap: usize) -> u64 {
    field.heap_bytes() as u64 + value_cap as u64 + HASH_SLOT_BYTES
}

/// Per-member delta a new set member charges. Mirrors [`hash_field_weight`]
/// for the set variant (no separate value, single bucket slot).
#[inline]
pub fn set_member_weight(member: &SmallBytes) -> u64 {
    member.heap_bytes() as u64 + SET_SLOT_BYTES
}

/// Per-item delta a new list element charges (Vec header slot + heap cap).
#[inline]
pub fn list_item_weight(value_cap: usize) -> u64 {
    LIST_SLOT_BYTES + value_cap as u64
}

/// Per-member delta a new zset member charges: hash slot for `by_member` +
/// BTreeSet slot for `by_score` + the member's heap bytes.
#[inline]
pub fn zset_member_weight(member: &SmallBytes) -> u64 {
    member.heap_bytes() as u64 + HASH_SLOT_BYTES + BTREE_SLOT_BYTES
}
