//! Value types — one backing structure per Redis type.
//!
//! ```
//! use kevy_store::{SetCondition, Store, Value};
//! let mut s = Store::new();
//! s.set(b"n", b"42".to_vec(), None, SetCondition::Always);
//! s.sadd(b"tags", &[b"a".as_slice()])?;
//! let mut types = Vec::new();
//! s.snapshot_each(|_, v: &Value, _| types.push(v.type_name()));
//! types.sort();
//! assert_eq!(types, ["set", "string"]);
//! # Ok::<(), kevy_store::StoreError>(())
//! ```

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::VecDeque;
use core::cmp::Ordering;
pub use kevy_bytes::SmallBytes;
use kevy_map::{KevyMap, KevySet};
use kevy_ranktree::RankTree;

/// Backing structure for a Hash value — [`KevyMap`] keyed by [`SmallBytes`]
/// (22 B inline / heap-else). Field names ≤22B (the vast majority — `name`,
/// `email`, etc.) live entirely inside the bucket, saving the 24 B Vec
/// metadata + heap allocation per field on a 22-byte budget.
///
/// ```
/// use kevy_store::{HashData, SmallBytes};
/// let mut h = HashData::new();
/// h.insert(SmallBytes::from_slice(b"name"), SmallBytes::from_slice(b"ada"));
/// assert_eq!(h.get(b"name".as_slice()).map(|v| v.as_ref()), Some(&b"ada"[..]));
/// ```
pub type HashData = KevyMap<SmallBytes, SmallBytes>;
/// Backing structure for a List value (a ring-buffer deque — O(1) both ends).
///
/// ```
/// use kevy_store::ListData;
/// let mut l = ListData::new();
/// l.push_back(b"b".to_vec());
/// l.push_front(b"a".to_vec());
/// assert_eq!(l, [b"a".to_vec(), b"b".to_vec()]);
/// ```
pub type ListData = VecDeque<Vec<u8>>;
/// Backing structure for a Set value — [`KevySet`] of [`SmallBytes`].
///
/// ```
/// use kevy_store::{SetData, SmallBytes};
/// let mut s = SetData::new();
/// assert!(s.insert(SmallBytes::from_slice(b"m")));
/// assert!(!s.insert(SmallBytes::from_slice(b"m")), "members are unique");
/// assert!(s.contains(b"m".as_slice()));
/// ```
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
///
/// ```
/// use kevy_store::Score;
/// assert!(Score::new(-0.0) < Score::new(0.0));
/// assert_ne!(Score::new(-0.0), Score::new(0.0));
/// assert_eq!(Score::new(1.5).value(), 1.5);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Score(pub(crate) f64);

impl Score {
    /// Wrap a score.
    pub const fn new(score: f64) -> Self {
        Self(score)
    }

    /// The score as a float.
    pub const fn value(self) -> f64 {
        self.0
    }
}
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
///
/// ```
/// use kevy_store::ScoreBound;
/// let lo = ScoreBound::exclusive(1.0);
/// assert!(lo.exclusive && lo.value == 1.0);
/// assert!(!ScoreBound::inclusive(f64::INFINITY).exclusive);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ScoreBound {
    /// The score itself. `f64::INFINITY` / `NEG_INFINITY` carry `+inf` and
    /// `-inf`, which is why this is not an `Option`.
    ///
    /// ```
    /// use kevy_store::ScoreBound;
    /// assert_eq!(ScoreBound::inclusive(2.5).value, 2.5);
    /// assert_eq!(ScoreBound::inclusive(f64::NEG_INFINITY).value, f64::NEG_INFINITY, "-inf");
    /// ```
    pub value: f64,
    /// `true` for Redis's `(` prefix — the endpoint is excluded from the
    /// range. An exclusive infinity is accepted and means the same as an
    /// inclusive one, since nothing equals infinity.
    ///
    /// ```
    /// use kevy_store::{ScoreBound, Store};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b")])?;
    /// let (lo, hi) = (ScoreBound::inclusive(1.0), ScoreBound::inclusive(2.0));
    /// assert_eq!(s.zcount(b"z", lo, hi)?, 2);
    /// assert_eq!(s.zcount(b"z", ScoreBound::exclusive(1.0), hi)?, 1, "(1 drops a");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub exclusive: bool,
}
impl ScoreBound {
    /// An endpoint the range includes (`ZRANGEBYSCORE`'s bare form).
    pub const fn inclusive(value: f64) -> Self {
        Self { value, exclusive: false }
    }

    /// An endpoint the range excludes (Redis's `(` prefix).
    pub const fn exclusive(value: f64) -> Self {
        Self { value, exclusive: true }
    }

    /// Does `s` satisfy this as a *minimum* bound?
    pub(crate) fn ge_ok(&self, s: f64) -> bool {
        if self.exclusive { s > self.value } else { s >= self.value }
    }
    /// Does `s` satisfy this as a *maximum* bound?
    pub(crate) fn le_ok(&self, s: f64) -> bool {
        if self.exclusive { s < self.value } else { s <= self.value }
    }
}

/// Sorted set: a member→score map plus an order-statistic B-tree keyed by
/// `(score, member)` ([`kevy_ranktree::RankTree`] — every node carries its
/// subtree count), so rank queries (`ZRANK`, `ZRANGE` by rank, `ZCOUNT`,
/// score-bound seeks) are O(log N) descents instead of linear walks.
///
/// ```
/// use kevy_store::{Store, Value};
/// let mut s = Store::new();
/// let members: Vec<(f64, &[u8])> =
///     [(3.0, &b"c"[..]), (1.0, b"a"), (2.0, b"b"), (4.0, b"d"), (5.0, b"e")].to_vec();
/// s.zadd(b"z", &members)?;
/// let mut order = Vec::new();
/// s.snapshot_each(|_, v, _| {
///     if let Value::ZSet(z) = v {
///         order = z.ordered().map(|(m, _)| m.to_vec()).collect();
///     }
/// });
/// assert_eq!(order, [b"a", b"b", b"c", b"d", b"e"]);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Default, Clone)]
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
                // An unchanged score has nothing to reorder. Without this,
                // the remove below takes (old, member) out of the index and
                // the insert at the bottom puts (score, member) back — the
                // same key, so the tree ends exactly where it began, having
                // paid a descent, a removal, an insertion and two extra
                // SmallBytes for it. Measured at 30.6% of the operation
                // on an 8,000-member set — see the ZADD decomposition
                // under bench/, which prices it against ZSCORE.
                //
                // Through `Score`, never `f64`. The two disagree on -0.0,
                // the tree keys on `Score`, and `==` here would skip a real
                // reordering and leave this map and that tree holding
                // different scores for one member.
                if Score(old) == Score(score) {
                    return false;
                }
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

#[cfg(feature = "std")]
pub use crate::value_enum::BioDropSender;
pub use crate::value_enum::{BULK_THRESHOLD, HEAP_HEAVY_BYTES, Value};

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
///
/// ```
/// use kevy_store::{ENTRY_OVERHEAD, SetCondition, Store};
/// let mut s = Store::new();
/// s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
/// // an inline key and value cost the entry overhead and nothing more
/// assert_eq!(s.used_memory(), ENTRY_OVERHEAD);
/// ```
pub const ENTRY_OVERHEAD: u64 = 96;

#[inline]
pub(crate) fn collection_overhead(capacity: usize, per_slot: u64) -> u64 {
    (capacity as u64).saturating_mul(per_slot)
}

/// What a new hash field adds to its hash's weight besides the table: the
/// allocator's footprint of the field name's heap and of the value's heap
/// (`value_heap` bytes), each 0 when it is short enough to sit inline in
/// the slot. The slot itself is part of the table, which is charged as the
/// table grows rather than per field.
///
/// ```
/// use kevy_store::{SmallBytes, hash_field_weight};
/// let short = SmallBytes::from_slice(b"name");
/// let long = SmallBytes::from_slice(&[b'f'; 64]);
/// // an inline field name and value add nothing
/// assert_eq!(hash_field_weight(&short, 0), 0);
/// // a spilled one adds what the allocator holds for it: 64 bytes asked, 80 held
/// assert_eq!(hash_field_weight(&long, 0), 80);
/// assert_eq!(hash_field_weight(&short, 900), 912);
/// ```
#[inline]
pub fn hash_field_weight(field: &SmallBytes, value_heap: usize) -> u64 {
    (kevy_map::malloc_footprint(field.heap_bytes()) + kevy_map::malloc_footprint(value_heap)) as u64
}

/// Per-member delta a new set member charges. Mirrors [`hash_field_weight`]
/// for the set variant (no separate value, single bucket slot).
///
/// ```
/// use kevy_store::{SmallBytes, set_member_weight};
/// let inline = SmallBytes::from_slice(b"m");
/// let spilled = SmallBytes::from_slice(&[b'm'; 64]);
/// assert_eq!(
///     set_member_weight(&spilled) - set_member_weight(&inline),
///     spilled.heap_bytes() as u64
/// );
/// ```
#[inline]
pub fn set_member_weight(member: &SmallBytes) -> u64 {
    member.heap_bytes() as u64 + SET_SLOT_BYTES
}

/// Per-item delta a new list element charges (Vec header slot + heap cap).
///
/// ```
/// use kevy_store::list_item_weight;
/// // the slot cost is fixed; the element's capacity is added on top
/// assert_eq!(list_item_weight(100) - list_item_weight(0), 100);
/// ```
#[inline]
pub fn list_item_weight(value_cap: usize) -> u64 {
    LIST_SLOT_BYTES + value_cap as u64
}

/// Per-member delta a new zset member charges: hash slot for `by_member` +
/// rank-tree slot for `by_score` + the member's heap bytes — twice, because
/// a heap-spilling member (>22 B) is stored in both structures (inline
/// members cost 0 here, matching [`Value::weight`]'s ZSet arm).
///
/// ```
/// use kevy_store::{SmallBytes, zset_member_weight};
/// let inline = SmallBytes::from_slice(b"m");
/// let spilled = SmallBytes::from_slice(&[b'm'; 64]);
/// // a spilled member is stored twice: in the map and in the rank tree
/// assert_eq!(
///     zset_member_weight(&spilled) - zset_member_weight(&inline),
///     2 * spilled.heap_bytes() as u64
/// );
/// ```
#[inline]
pub fn zset_member_weight(member: &SmallBytes) -> u64 {
    2 * member.heap_bytes() as u64 + HASH_SLOT_BYTES + RANKTREE_SLOT_BYTES
}
