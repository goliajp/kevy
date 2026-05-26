//! Value types — one backing structure per Redis type.

pub use kevy_bytes::SmallBytes;
use kevy_map::{KevyMap, KevySet};
use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};

/// Backing structure for a Hash value — [`KevyMap`] (open-addressing Swiss
/// table, kevy-hash one-call hasher, no DoS hardening — same-shaped wins as
/// the keyspace map).
pub type HashData = KevyMap<Vec<u8>, Vec<u8>>;
/// Backing structure for a List value (a ring-buffer deque — O(1) both ends).
pub type ListData = VecDeque<Vec<u8>>;
/// Backing structure for a Set value — [`KevySet`] wrapper over `KevyMap<K, ()>`.
pub type SetData = KevySet<Vec<u8>>;

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
#[derive(Default)]
pub struct ZSetData {
    pub(crate) by_member: KevyMap<Vec<u8>, f64>,
    pub(crate) by_score: BTreeSet<(Score, Vec<u8>)>,
}

impl ZSetData {
    pub(crate) fn insert(&mut self, member: &[u8], score: f64) -> bool {
        let is_new = match self.by_member.insert(member.to_vec(), score) {
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
/// The collection variants are **boxed** so the enum is only as big as `Str`
/// (24 B) + tag = 32 B, not the 56 B `ZSetData` — every `Entry` (incl. the
/// common string case) is then ~48 B instead of ~80 B, so the hashbrown bucket
/// array is ~40% denser/smaller (fewer cache misses on a large keyspace, less
/// RSS). The extra pointer-chase lands only on collection ops, not the hot
/// string GET path.
///
/// `Str` holds a [`SmallBytes`] (24 B, same size as `Vec<u8>`) so byte strings
/// up to 22 bytes live **inline inside the bucket**, killing the second cache
/// miss the value pointer-chase used to cost on large-keyspace GETs.
pub enum Value {
    Str(SmallBytes),
    Hash(Box<HashData>),
    List(Box<ListData>),
    Set(Box<SetData>),
    ZSet(Box<ZSetData>),
}

const _: () = {
    // Don't let future variants undo box-collection's Entry-48B win.
    assert!(std::mem::size_of::<Value>() <= 32);
};

impl Value {
    /// The Redis type name (`TYPE` command).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "string",
            Value::Hash(_) => "hash",
            Value::List(_) => "list",
            Value::Set(_) => "set",
            Value::ZSet(_) => "zset",
        }
    }
}
