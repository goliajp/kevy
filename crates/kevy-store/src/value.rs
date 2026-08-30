//! Value types — one backing structure per Redis type.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
pub use kevy_bytes::SmallBytes;
use kevy_map::{KevyMap, KevySet};
use kevy_ranktree::RankTree;
use core::cmp::Ordering;
use alloc::collections::VecDeque;
use alloc::sync::Arc;

/// Backing structure for a Hash value — [`KevyMap`] keyed by [`SmallBytes`]
/// (22 B inline / heap-else). Field names ≤22B (the vast majority — `name`,
/// `email`, etc.) live entirely inside the bucket, saving the 24 B Vec
/// metadata + heap allocation per field on a 22-byte budget.
pub type HashData = KevyMap<SmallBytes, SmallBytes>;
/// Backing structure for a List value (a ring-buffer deque — O(1) both ends).
pub type ListData = VecDeque<Vec<u8>>;
/// Backing structure for a Set value — [`KevySet`] of [`SmallBytes`].
pub type SetData = KevySet<SmallBytes>;

/// A total-ordered f64 score (Redis scores are never NaN). `total_cmp` gives a
/// total order so scores can key an ordered container.
///
/// `PartialEq` is written rather than derived, and it must stay that way.
/// Derived, it is `f64`'s `==`, which disagrees with the `total_cmp` below
/// on `-0.0`: `==` calls it equal to `0.0`, `total_cmp` orders it before.
/// Rust requires of an `Ord` key that `a == b` exactly when `a.cmp(b)` is
/// `Equal`, and a `Score` that breaks that is a key whose container and
/// whose callers disagree about which entries are the same one.
///
/// It was latent while nothing compared two `Score`s — `ZSetData::insert`
/// reached the tree only through `Ord`, and its unconditional
/// remove-then-insert never had to ask. `ZADD z -0 m` is accepted and
/// `ZADD z2 -0 a; ZADD z2 0 b` really does order `a` first, so the first
/// caller to write `old == score` would have skipped a live update and left
/// `by_member` and `by_score` holding different scores for one member.
/// NaN cannot arrive: the parser refuses it.
#[derive(Clone, Copy)]
pub struct Score(pub f64);
impl PartialEq for Score {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}
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
    /// The score itself. `f64::INFINITY` / `NEG_INFINITY` carry `+inf` and
    /// `-inf`, which is why this is not an `Option`.
    pub value: f64,
    /// `true` for Redis's `(` prefix — the endpoint is excluded from the
    /// range. An exclusive infinity is accepted and means the same as an
    /// inclusive one, since nothing equals infinity.
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

/// Sorted set: a member→score map plus an order-statistic B-tree keyed by
/// `(score, member)` ([`kevy_ranktree::RankTree`] — every node carries its
/// subtree count), so rank queries (`ZRANK`, `ZRANGE` by rank, `ZCOUNT`,
/// score-bound seeks) are O(log N) descents instead of linear walks.
#[derive(Default, Clone)]
pub struct ZSetData {
    pub(crate) by_member: KevyMap<SmallBytes, f64>,
    /// The `(score, member)` order-statistic index. Member is a
    /// [`SmallBytes`] (≤22 B inline in the node's key slot), ordered by
    /// byte-lexicographic `Ord` — the same order the old `Vec<u8>` gave.
    pub(crate) by_score: RankTree<(Score, SmallBytes)>,
}

impl ZSetData {
    pub(crate) fn insert(&mut self, member: &[u8], score: f64) -> bool {
        let is_new = match self.by_member.insert(SmallBytes::from_slice(member), score) {
            Some(old) => {
                self.by_score.remove(&(Score(old), SmallBytes::from_slice(member)));
                false
            }
            None => true,
        };
        self.by_score.insert((Score(score), SmallBytes::from_slice(member)));
        is_new
    }
    pub(crate) fn remove(&mut self, member: &[u8]) -> bool {
        match self.by_member.remove(member) {
            Some(old) => {
                self.by_score.remove(&(Score(old), SmallBytes::from_slice(member)));
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
    /// Like [`Self::ordered`] but starting at ascending `rank` — one
    /// O(log N) seek, no skip-walk.
    pub(crate) fn ordered_from(&self, rank: usize) -> impl Iterator<Item = (&[u8], f64)> {
        self.by_score.iter_from(rank).map(|(s, m)| (m.as_slice(), s.0))
    }
    /// The ascending rank of `member` (whose score is `score`). O(log N).
    pub(crate) fn rank_of(&self, member: &[u8], score: f64) -> Option<usize> {
        self.by_score.rank_of(&(Score(score), SmallBytes::from_slice(member)))
    }
    /// First rank whose score satisfies `min` as a lower bound. O(log N).
    pub(crate) fn score_start_rank(&self, min: &ScoreBound) -> usize {
        self.by_score.partition_point(|(s, _)| !min.ge_ok(s.0))
    }
    /// First rank whose score fails `max` as an upper bound (i.e. one past
    /// the last in-range rank). O(log N).
    pub(crate) fn score_end_rank(&self, max: &ScoreBound) -> usize {
        self.by_score.partition_point(|(s, _)| max.le_ok(s.0))
    }
}

pub use crate::value_cold::{COLD_TAG_HASH, COLD_TAG_STRING, ColdRef};

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
    /// A byte string, inline up to 22 bytes — see the type doc above for
    /// why that boundary is where it is.
    Str(SmallBytes),
    /// Following valkey's OBJ_ENCODING_INT: when a SET
    /// stores a clean canonical i64 ASCII string (parses round-trip), we
    /// keep the integer **as i64** rather than as 22 B of inline bytes.
    /// Wins on INCR (in-place `+= delta`, no parse / no format / no
    /// SmallBytes wrap) and on memory (8 B vs 24 B). GET formats it via
    /// a per-`Store` scratch buffer.
    Int(i64),
    /// Values larger
    /// than [`BULK_THRESHOLD`] bytes get stored behind an
    /// `Arc<Box<[u8]>>` instead of a heap-backed `SmallBytes`. The Arc
    /// lets the io_uring reactor's reply path borrow the bytes across
    /// the SQE→CQE window safely (Arc clone keeps them alive even if
    /// the keyspace mutates) — the prerequisite for the writev
    /// zero-copy bulk reply path, which skips the per-GET memcpy from
    /// value storage into the per-conn output buffer.
    ///
    /// **Why `Arc<Box<[u8]>>` and not `Arc<[u8]>`**: `Arc<[u8]>` is a
    /// DST-backed `ArcInner<[u8]> = { strong, weak, [u8; N] }` whose
    /// data slot sits past the refcount words. `Arc::from(Vec<u8>)`
    /// allocates a fresh `ArcInner` and `copy_from_slice`s the bytes
    /// — a hard mandatory 64 KiB memcpy on every big SET. With
    /// `Arc<Box<[u8]>>`, the `Box<[u8]>` wrapper occupies the Arc's
    /// data slot (16 B), pointing AT an unchanged heap buffer; so
    /// `Arc::new(vec.into_boxed_slice())` is **truly zero-copy**
    /// (the boxed slice's allocation stays put — only the 32-byte
    /// `ArcInner` is freshly malloced). Per-GET cost: one extra
    /// pointer dereference (`&**arc` to get `&[u8]`), measured to be
    /// negligible vs the per-SET memcpy savings. The `Arc<[u8]>`
    /// mandatory copy was confirmed with perf-record before switching
    /// to `Arc<Box<[u8]>>`.
    ///
    /// Small values stay on `Str(SmallBytes)` because the inline
    /// cache-line storage beats an Arc indirection for the common case.
    ArcBulk(Arc<Box<[u8]>>),
    /// A hash below [`HS_PROMOTE`] elements: one map behind one `Arc`, so
    /// a snapshot pins it whole and the first write during that window
    /// deep-clones it. Past that size it becomes `SegHash`.
    Hash(Arc<HashData>),
    /// A hash past `seg_map::HS_PROMOTE` fields: an extendible-hash
    /// directory of `Arc`-shared buckets — a COW write under a live
    /// snapshot view clones one bucket, not the whole value.
    SegHash(Arc<crate::seg_map::SegMap<SmallBytes>>),
    /// A list below [`SEG_PROMOTE`] elements: one deque behind one `Arc`,
    /// with the same whole-value copy-on-write. Past that size it becomes
    /// `SegList`.
    List(Arc<ListData>),
    /// A list past [`crate::list_seg::SEG_PROMOTE`] elements: a deque of
    /// `Arc`-shared segments so a COW write under a live snapshot view
    /// clones one segment, not the whole (possibly multi-GB) value. See
    /// `list_seg.rs` for the promotion contract.
    SegList(Arc<crate::list_seg::SegListData>),
    /// A set below [`HS_PROMOTE`] elements, on the same terms as `Hash`.
    Set(Arc<SetData>),
    /// A set past `seg_map::HS_PROMOTE` members — the set door of the
    /// same bucket-sharded COW as [`Value::SegHash`].
    SegSet(Arc<crate::seg_map::SegMap<()>>),
    /// A sorted set: members with scores, plus the order-statistic tree
    /// that makes rank queries a lookup rather than a scan.
    ZSet(Arc<ZSetData>),
    /// A zset past `zset_seg::Z_PROMOTE` members — sharded member map
    /// + ordered segments; COW writes clone one bucket + one segment.
    SegZSet(Arc<crate::zset_seg::SegZSetData>),
    /// A stream: entries, consumer groups and their pending lists. Never
    /// segmented — a stream trims from the front instead of growing
    /// without bound.
    Stream(Arc<crate::stream::StreamData>),
    /// Valkey-orthodox encoding switch: tiny sets (1-N
    /// short members) live inline in 24 bytes instead of behind
    /// `Arc<SetData>` — matches valkey's `OBJ_ENCODING_LISTPACK` for
    /// sets, which is what `redis-benchmark -t sadd` default `-r 0`
    /// (cardinality stays at 1 forever, single 20-byte literal member)
    /// measures. On overflow ([`crate::small_set::SmallSetData::try_add`]
    /// returns `NoRoom`) the set is promoted to `Value::Set(Arc<SetData>)`
    /// — the Swiss-table path that wins for larger cardinalities.
    SmallSetInline(crate::small_set::SmallSetData),
    /// Tiny hashes
    /// (1-2 short field-value pairs) live inline in 24 bytes; promoted
    /// to `Value::Hash(Arc<HashData>)` on overflow. Mirrors valkey's
    /// `OBJ_ENCODING_LISTPACK` for hashes.
    SmallHashInline(crate::small_hash::SmallHashData),
    /// A declared table's row: the columns in declared order, in one payload
    /// buffer, with no field names and no per-row table.
    ///
    /// Reachable only for a key under a declared prefix — an undeclared hash
    /// keeps [`Value::Hash`].
    PackedRow(crate::packed_row::PackedRow),
    /// Tiny lists inline encoding; promoted to
    /// `Value::List(Arc<ListData>)` on overflow.
    SmallListInline(crate::small_list::SmallListData),
    /// Tiny sorted sets inline encoding; promoted to
    /// `Value::ZSet(Arc<ZSetData>)` on overflow.
    SmallZSetInline(crate::small_zset::SmallZSetData),
    /// A demoted (tiered-to-disk) value's in-map stub. The two-stage
    /// funnel (`tier` module) resolves this before any typed match sees
    /// it: stage 1 answers existence/TYPE/TTL from the stub with zero
    /// IO; stage 2 materializes (serve or promote) only on a type
    /// match. Cloning a `Cold` clones the STUB, not the record — paths
    /// that duplicate values (COPY, cross-shard ship) materialize
    /// first so two stubs never alias one vlog record.
    Cold(ColdRef),
}

/// Threshold (bytes) above which a SET stores its value as
/// [`Value::ArcBulk`] (writev-eligible on GET) instead of [`Value::Str`]
/// (inline `SmallBytes`). 64 B ≈ one cache line — below that the
/// inline-SmallBytes storage wins on L1 locality; above it the
/// writev-borrow win dominates.
pub const BULK_THRESHOLD: usize = 64;

const _: () = {
    // Don't let future variants undo box-collection's Entry-48B win.
    assert!(core::mem::size_of::<Value>() <= 32);
};

/// Heap-size threshold above which an overwritten `Value` is sent to the
/// runtime's bio thread for off-reactor drop instead of being freed inline
/// (lazy-drop).
///
/// **Why not lower**: a 256 B threshold regressed c=50 -d 10240 SET
/// p999 from 0.487 → 1.583 ms (worse by 3.25×). The cause: `std::sync::mpsc::Sender::send`
/// is a few hundred ns of atomic + Box clone, which EXCEEDS the inline
/// `Box::<[u8]>::drop` cost when the allocator serves the free from a
/// hot large-class slab (~ 1-3 µs for 10 KB; the bench's steady state).
/// Off-loading only wins when the inline drop's tail risk (cold-slab
/// `munmap`/`madvise` consolidation stall, observed at 50-150 µs and
/// occasionally millisecond-range) exceeds the per-send channel cost
/// PLUS the cross-thread cache-line bouncing.
///
/// With per-shard batch accumulation flushing at the end of every
/// reactor iteration, the per-mpsc-send cost is amortised across N
/// drops. That makes the channel hop profitable at smaller sizes than
/// a lone-send model could justify (lone-send had to lift the
/// threshold to 16 KB because per-`mpsc::send` cost was a few hundred
/// ns — at 256 B the inline drop was cheaper).
///
/// **Sweet-spot surprise**: intuition suggested dropping the threshold
/// to 256 B – 1 KB once batching amortises the send. A sweep across
/// thresholds {512, 1024, 4096, 16384} × c=50 SET -d {1K, 4K, 10K, 64K}
/// disproved that floor: at ≤ 1 KB threshold, p999 / max on small
/// values (-d 1024, -d 4096) was variance-bounded equal or
/// occasionally WORSE than a 16 KB threshold, while the larger
/// sizes (10 KB / 64 KB) won either way. Cause: the Vec::push +
/// occasional `MAX_PENDING_DROPS` force-flush stall costs more for
/// small Arcs (allocator small-class free is sub-µs even at tail)
/// than the inline drop it avoids.
///
/// Picked **4 KB** as the lowest threshold where the bio-off-reactor
/// win consistently dominates the batch-buffer overhead on tail
/// metrics. The biggest batching wins (vs lone-send at 16 KB) land on
/// `-d 64K` SET p50 (-44 %) and `-d 10K` SET max (-35 %), where each
/// iter's batch already contains several heavy values per shard.
pub const HEAP_HEAVY_BYTES: usize = 4 * 1024;

/// Sender half of the runtime's bio-drop channel. Wired from
/// `kevy-rt`'s `bio.rs` via [`crate::Store::set_bio_drop_sender`]; the
/// concrete payload is `Vec<Value>` — a **batch** of values
/// produced by one shard since its last flush.
/// The bio thread (`kevy-rt::bio::spawn`) iterates the batch and
/// drops each item. One mpsc message per shard-flush amortises the
/// channel cost (atomic + cross-thread cacheline traffic) across
/// however many values landed in the batch.
#[cfg(feature = "std")]
pub type BioDropSender = std::sync::mpsc::Sender<Vec<Value>>;

impl Value {
    /// The Redis type name (`TYPE` command).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) | Value::Int(_) | Value::ArcBulk(_) => "string",
            Value::Hash(_) | Value::SegHash(_) | Value::SmallHashInline(_) | Value::PackedRow(_) => "hash",
            Value::List(_) | Value::SegList(_) | Value::SmallListInline(_) => "list",
            Value::Set(_) | Value::SegSet(_) | Value::SmallSetInline(_) => "set",
            Value::ZSet(_) | Value::SegZSet(_) | Value::SmallZSetInline(_) => "zset",
            Value::Stream(_) => "stream",
            // Stage-1 funnel: TYPE (and SCAN's TYPE filter) answer from
            // the tag — a cold key never pays a pread for its type.
            Value::Cold(c) => c.type_name(),
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
/// `BTreeSet`/`BTreeMap` per-entry overhead (node pointers + B-tree node
/// padding) — the stream index's accounting constant.
pub(crate) const BTREE_SLOT_BYTES: u64 = 40;
/// `kevy_ranktree::RankTree` per-key overhead. Measured from the structure:
/// the `(Score, SmallBytes)` key slot is 32 B; nodes hold ≤15 keys in a Vec
/// whose buffer rounds to 16 slots at ~2/3 fill (≈10-11 live keys), so the
/// key arrays amortise to ≈48 B per key; the per-node fixed cost (56 B
/// header + Box allocation, ~1 node per 10 keys) and the internal nodes'
/// child-pointer arrays add ≈8 B more. 64 errs slightly high (allocator
/// size-class rounding), keeping `used_memory` a conservative upper bound —
/// same policy as [`ENTRY_OVERHEAD`].
pub(crate) const RANKTREE_SLOT_BYTES: u64 = 64;
/// Per-entry overhead in the top-level keyspace map: the inline 24-byte
/// `SmallBytes` key cell + the 64-byte `Entry` (post weight/clock fields) +
/// metadata. Approximation that errs slightly high so `used_memory` stays a
/// conservative upper bound vs the actual allocator footprint.
pub const ENTRY_OVERHEAD: u64 = 96;

#[inline]
pub(crate) fn collection_overhead(capacity: usize, per_slot: u64) -> u64 {
    (capacity as u64).saturating_mul(per_slot)
}

/// Per-field delta a new hash field charges against the entry weight: heap
/// bytes for the field name (if not inline) + heap bytes for the value (0 when
/// the value is ≤22 B and lives inline in the slot) + one slot of bucket
/// overhead. Used when an HSET inserts a brand-new field. Both field and value
/// inline in the fixed-size slot when short, so only the off-slot footprint is
/// charged — symmetric with [`set_member_weight`].
#[inline]
pub fn hash_field_weight(field: &SmallBytes, value_heap: usize) -> u64 {
    field.heap_bytes() as u64 + value_heap as u64 + HASH_SLOT_BYTES
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
/// rank-tree slot for `by_score` + the member's heap bytes — twice, because
/// a heap-spilling member (>22 B) is stored in both structures (inline
/// members cost 0 here, matching [`Value::weight`]'s ZSet arm).
#[inline]
pub fn zset_member_weight(member: &SmallBytes) -> u64 {
    2 * member.heap_bytes() as u64 + HASH_SLOT_BYTES + RANKTREE_SLOT_BYTES
}

#[cfg(test)]
mod score_order_tests {
    use super::Score;
    use core::cmp::Ordering;

    /// `Eq` and `Ord` must agree, which is what Rust requires of a key in an
    /// ordered container and what a derived `PartialEq` does not give here.
    /// `-0.0` is the case that matters: `ZADD z -0 m` is accepted, and a
    /// sorted set really does order `-0.0` before `0.0`.
    ///
    /// `assert!` rather than `assert_eq!` because `Score` carries no `Debug`
    /// and a hot key type should not grow one to please a test.
    #[test]
    fn eq_agrees_with_ord() {
        let interesting = [
            0.0_f64, -0.0, 1.0, -1.0, f64::MIN, f64::MAX,
            f64::INFINITY, f64::NEG_INFINITY, 1e-308, -1e-308,
        ];
        for &a in &interesting {
            for &b in &interesting {
                let (sa, sb) = (Score(a), Score(b));
                assert!(
                    (sa == sb) == (sa.cmp(&sb) == Ordering::Equal),
                    "Eq and Ord disagree on {a} vs {b}",
                );
            }
        }
    }

    /// The specific pair that made this worth writing down: equal under
    /// `f64`'s `==`, distinct under the order the rank tree keys on.
    #[test]
    fn negative_zero_is_not_positive_zero() {
        assert!(Score(-0.0) != Score(0.0), "-0.0 must not equal 0.0");
        assert!(Score(-0.0).cmp(&Score(0.0)) == Ordering::Less);
        assert!(Score(-0.0) == Score(-0.0));
        assert!(-0.0_f64 == 0.0_f64, "the f64 == this type must not use");
    }
}
