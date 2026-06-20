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
    Hash(Arc<HashData>),
    List(Arc<ListData>),
    Set(Arc<SetData>),
    ZSet(Arc<ZSetData>),
    Stream(Arc<crate::stream::StreamData>),
}

const _: () = {
    // Don't let future variants undo box-collection's Entry-48B win.
    assert!(std::mem::size_of::<Value>() <= 32);
};

impl Value {
    /// The Redis type name (`TYPE` command).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) | Value::Int(_) => "string",
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
}

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
