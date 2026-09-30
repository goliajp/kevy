//! Segmented sorted set — element-granularity COW for giant zsets.
//!
//! A zset past [`Z_PROMOTE`] members stops being one `Arc<ZSetData>`
//! and becomes `Value::SegZSet`: the member→score side is a
//! [`SegMap`] (the Stage-HS bucket-sharded stone), and the
//! score-ordered side is a vector of `Arc`-shared [`RankTree`]
//! segments holding contiguous `(score, member)` ranges of ≤
//! [`ZSEG_CAP`] entries. A snapshot view pins everything with one
//! outer Arc clone; the first write during that window clones one
//! member bucket plus one segment tree — never the whole value. This
//! reuses both existing stones untouched (no fork of the B-tree's
//! rebalancing internals); the price is an O(segments) prefix walk on
//! rank arithmetic, microseconds even at hundreds of millions of
//! members. Design rationale: the element-COW RFC under
//! the element-COW RFC.
//!
//! ```
//! use kevy_store::zset_seg::SegZSetData;
//! let mut z = SegZSetData::default();
//! z.insert(b"b", 2.0);
//! z.insert(b"a", 1.0);
//! let view = z.clone(); // pins the current contents
//! z.insert(b"a", 3.0);
//! assert_eq!(view.score_of(b"a"), Some(1.0));
//! assert_eq!(z.ordered().map(|(m, _)| m).collect::<Vec<_>>(), [&b"b"[..], b"a"]);
//! ```

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::seg_map::SegMap;
use crate::value::{Score, ScoreBound, SmallBytes};
use alloc::sync::Arc;
use kevy_ranktree::RankTree;

/// Flat `Value::ZSet` size at which a write promotes to the segmented
/// representation.
///
/// ```
/// use kevy_store::Store;
/// use kevy_store::zset_seg::Z_PROMOTE;
/// let mut s = Store::new();
/// let members: Vec<Vec<u8>> = (0..=Z_PROMOTE).map(|i| i.to_string().into_bytes()).collect();
/// let pairs: Vec<(f64, &[u8])> = members.iter().map(|m| (1.0, m.as_slice())).collect();
/// assert_eq!(s.zadd(b"z", &pairs)?, Z_PROMOTE + 1);
/// assert_eq!(s.zcard(b"z")?, Z_PROMOTE + 1);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
pub const Z_PROMOTE: usize = 16 * 1024;
/// Entries per score-ordered segment tree; one segment is the per-write
/// COW clone bound, and — as with `seg_map::BUCKET_SPLIT` — the grain
/// score-scattered write bursts aggregate over per tick. 2K entries
/// keeps a burst's per-tick clone total under the tick bar (empirically
/// sized alongside `BUCKET_SPLIT` — see its note).
///
/// ```
/// use kevy_store::zset_seg::{SegZSetData, ZSEG_CAP};
/// let mut z = SegZSetData::default();
/// for i in 0..3 * ZSEG_CAP {
///     z.insert(&i.to_be_bytes(), -(i as f64));
/// }
/// // spread over several segments, the order is still global
/// let first = z.ordered().next().unwrap();
/// assert_eq!(first.1, -((3 * ZSEG_CAP - 1) as f64));
/// ```
pub const ZSEG_CAP: usize = 512;

type ZKey = (Score, SmallBytes);

/// A giant sorted set: sharded member→score map + ordered segment
/// trees. Segments are non-empty and range-disjoint; `maxes[i]` caches
/// `segs[i]`'s largest key for O(log segments) routing.
///
/// ```
/// use kevy_store::zset_seg::SegZSetData;
/// let mut z = SegZSetData::default();
/// z.insert(b"low", 1.0);
/// z.insert(b"high", 9.0);
/// assert_eq!(z.rank_of(b"high", 9.0), Some(1));
/// ```
#[derive(Debug, Clone, Default)]
pub struct SegZSetData {
    by_member: SegMap<f64>,
    segs: Vec<Arc<RankTree<ZKey>>>,
    maxes: Vec<ZKey>,
}

impl SegZSetData {
    #[inline]
    /// Members across every segment, as a running count.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// z.insert(b"a", 1.0);
    /// z.insert(b"a", 2.0); // an update, not a new member
    /// assert_eq!(z.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.by_member.len()
    }

    #[inline]
    /// Whether the sorted set holds no members.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// assert!(z.is_empty());
    /// z.insert(b"a", 1.0);
    /// assert!(!z.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.by_member.is_empty()
    }

    /// One member's score, or `None` if it is not present. A lookup
    /// through the member index, not a walk of the score order.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// z.insert(b"a", 1.5);
    /// assert_eq!(z.score_of(b"a"), Some(1.5));
    /// assert_eq!(z.score_of(b"b"), None);
    /// ```
    pub fn score_of(&self, member: &[u8]) -> Option<f64> {
        self.by_member.get(member).copied()
    }

    /// Membership, on the same index path as `score_of` and without
    /// reading the score.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// z.insert(b"a", 1.0);
    /// assert!(z.contains_member(b"a"));
    /// assert!(!z.contains_member(b"b"));
    /// ```
    pub fn contains_member(&self, member: &[u8]) -> bool {
        self.by_member.contains_key(member)
    }

    /// Segment index a key routes to for insertion (first segment whose
    /// max is ≥ the key; past-the-end keys go to the last segment).
    fn route(&self, key: &ZKey) -> usize {
        let i = self.maxes.partition_point(|mx| mx < key);
        i.min(self.segs.len().saturating_sub(1))
    }

    /// Insert or update; returns whether the member was new. COW cost:
    /// one member bucket + one segment tree.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// assert!(z.insert(b"a", 1.0)); // new
    /// assert!(!z.insert(b"a", 2.0)); // score update
    /// assert_eq!(z.score_of(b"a"), Some(2.0));
    /// ```
    pub fn insert(&mut self, member: &[u8], score: f64) -> bool {
        let smb = SmallBytes::from_slice(member);
        let old = self.by_member.insert(smb.clone(), score);
        if let Some(old_sc) = old {
            // See ZSetData::insert. Same reasoning, and one cost more: the
            // path below reaches its segment through Arc::make_mut, so under
            // a live snapshot an unchanged score deep-clones a segment of up
            // to ZSEG_CAP entries in order to put back what was in it.
            if Score(old_sc) == Score(score) {
                return false;
            }
            self.remove_ordered(&(Score(old_sc), smb.clone()));
        }
        let key = (Score(score), smb);
        if self.segs.is_empty() {
            let mut t = RankTree::new();
            t.insert(key.clone());
            self.segs.push(Arc::new(t));
            self.maxes.push(key);
            return old.is_none();
        }
        let si = self.route(&key);
        let seg = Arc::make_mut(&mut self.segs[si]);
        seg.insert(key.clone());
        if key > self.maxes[si] {
            self.maxes[si] = key;
        }
        if seg.len() > ZSEG_CAP {
            self.split(si);
        }
        old.is_none()
    }

    /// Remove a member; returns whether it was present.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// z.insert(b"a", 1.0);
    /// assert!(z.remove(b"a"));
    /// assert!(!z.remove(b"a"));
    /// ```
    pub fn remove(&mut self, member: &[u8]) -> bool {
        let Some(sc) = self.by_member.remove(member) else {
            return false;
        };
        self.remove_ordered(&(Score(sc), SmallBytes::from_slice(member)));
        true
    }

    /// Drop `key` from its segment, retiring emptied segments and
    /// refreshing the cached max when the tail key goes.
    fn remove_ordered(&mut self, key: &ZKey) {
        let si = self.route(key);
        let seg = Arc::make_mut(&mut self.segs[si]);
        seg.remove(key);
        if seg.is_empty() {
            self.segs.remove(si);
            self.maxes.remove(si);
        } else if *key == self.maxes[si] {
            self.maxes[si] = seg.iter_rev().next().expect("non-empty").clone();
        }
    }

    /// Split segment `si` (over [`ZSEG_CAP`]) into two rank halves.
    /// O(segment): both halves rebuild from the ordered walk.
    fn split(&mut self, si: usize) {
        let src = &self.segs[si];
        let half = src.len() / 2;
        let mut lo = RankTree::new();
        let mut hi = RankTree::new();
        for (i, k) in src.iter().enumerate() {
            if i < half {
                lo.insert(k.clone());
            } else {
                hi.insert(k.clone());
            }
        }
        let lo_max = lo.iter_rev().next().expect("half non-empty").clone();
        self.segs[si] = Arc::new(lo);
        self.segs.insert(si + 1, Arc::new(hi));
        self.maxes.insert(si, lo_max);
        // maxes[si + 1] keeps the old segment's max — still hi's max.
    }

    /// `(member, score)` pairs in ascending `(score, member)` order.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// z.insert(b"b", 2.0);
    /// z.insert(b"a", 2.0);
    /// z.insert(b"c", 1.0);
    /// let got: Vec<(&[u8], f64)> = z.ordered().collect();
    /// assert_eq!(got, [(&b"c"[..], 1.0), (b"a", 2.0), (b"b", 2.0)]);
    /// ```
    pub fn ordered(&self) -> impl Iterator<Item = (&[u8], f64)> {
        self.segs.iter().flat_map(|t| t.iter()).map(|(s, m)| (m.as_slice(), s.0))
    }

    /// Like [`Self::ordered`] but starting at ascending `rank` — an
    /// O(segments) prefix walk, then a seek inside the hit segment.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// for (m, s) in [(b"a", 1.0), (b"b", 2.0), (b"c", 3.0)] {
    ///     z.insert(m, s);
    /// }
    /// let tail: Vec<&[u8]> = z.ordered_from(1).map(|(m, _)| m).collect();
    /// assert_eq!(tail, [&b"b"[..], b"c"]);
    /// ```
    pub fn ordered_from(&self, rank: usize) -> impl Iterator<Item = (&[u8], f64)> {
        let (si, off) = self.locate_rank(rank);
        self.segs[si..]
            .iter()
            .enumerate()
            .flat_map(move |(j, t)| t.iter_from(if j == 0 { off } else { 0 }))
            .map(|(s, m)| (m.as_slice(), s.0))
    }

    /// Segment index + in-segment rank for a global rank. `rank >= len`
    /// yields `(segs.len(), 0)` — an empty tail.
    fn locate_rank(&self, rank: usize) -> (usize, usize) {
        let mut remaining = rank;
        for (si, t) in self.segs.iter().enumerate() {
            if remaining < t.len() {
                return (si, remaining);
            }
            remaining -= t.len();
        }
        (self.segs.len(), 0)
    }

    /// The ascending rank of `member` (whose score is `score`).
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// z.insert(b"a", 1.0);
    /// z.insert(b"b", 2.0);
    /// assert_eq!(z.rank_of(b"b", 2.0), Some(1));
    /// assert_eq!(z.rank_of(b"b", 5.0), None); // the score must match
    /// ```
    pub fn rank_of(&self, member: &[u8], score: f64) -> Option<usize> {
        let key = (Score(score), SmallBytes::from_slice(member));
        if self.segs.is_empty() {
            return None;
        }
        let si = self.route(&key);
        let base: usize = self.segs[..si].iter().map(|t| t.len()).sum();
        self.segs[si].rank_of(&key).map(|r| base + r)
    }

    /// First rank whose score satisfies `min` as a lower bound.
    ///
    /// ```
    /// use kevy_store::ScoreBound;
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// for (m, s) in [(b"a", 1.0), (b"b", 2.0), (b"c", 3.0)] {
    ///     z.insert(m, s);
    /// }
    /// assert_eq!(z.score_start_rank(&ScoreBound::inclusive(2.0)), 1);
    /// assert_eq!(z.score_start_rank(&ScoreBound::exclusive(2.0)), 2);
    /// ```
    pub fn score_start_rank(&self, min: &ScoreBound) -> usize {
        self.frontier_rank(|s| !min.ge_ok(s))
    }

    /// One past the last rank whose score satisfies `max` as an upper
    /// bound.
    ///
    /// ```
    /// use kevy_store::ScoreBound;
    /// use kevy_store::zset_seg::SegZSetData;
    /// let mut z = SegZSetData::default();
    /// for (m, s) in [(b"a", 1.0), (b"b", 2.0), (b"c", 3.0)] {
    ///     z.insert(m, s);
    /// }
    /// assert_eq!(z.score_end_rank(&ScoreBound::inclusive(2.0)), 2);
    /// assert_eq!(z.score_end_rank(&ScoreBound::exclusive(2.0)), 1);
    /// ```
    pub fn score_end_rank(&self, max: &ScoreBound) -> usize {
        self.frontier_rank(|s| max.le_ok(s))
    }

    /// Count of leading keys for which the (monotone) score predicate
    /// holds: whole segments answer from their cached max, the frontier
    /// segment does one O(log) partition descent.
    fn frontier_rank<F: Fn(f64) -> bool>(&self, pred: F) -> usize {
        let mut acc = 0usize;
        for (si, t) in self.segs.iter().enumerate() {
            if pred(self.maxes[si].0.0) {
                acc += t.len();
            } else {
                return acc + t.partition_point(|(s, _)| pred(s.0));
            }
        }
        acc
    }

    /// Build from the flat representation: ordered chunks become
    /// segment trees; members re-shard through the SegMap insert.
    ///
    /// ```
    /// use kevy_store::zset_seg::SegZSetData;
    /// use kevy_store::{Store, Value};
    /// let mut s = Store::new();
    /// s.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b"), (3.0, b"c")])?;
    /// let Some((Value::ZSet(flat), _)) = s.clone_with_ttl(b"z") else { unreachable!() };
    /// let seg = SegZSetData::from_flat(&flat);
    /// assert_eq!(seg.len(), 3);
    /// assert_eq!(seg.score_of(b"b"), Some(2.0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn from_flat(flat: &crate::value::ZSetData) -> Self {
        let mut out = SegZSetData::default();
        let mut cur = RankTree::new();
        for (m, sc) in flat.ordered() {
            let smb = SmallBytes::from_slice(m);
            out.by_member.insert(smb.clone(), sc);
            cur.insert((Score(sc), smb));
            if cur.len() == ZSEG_CAP {
                out.push_built_seg(&mut cur);
            }
        }
        if !cur.is_empty() {
            out.push_built_seg(&mut cur);
        }
        out
    }

    fn push_built_seg(&mut self, cur: &mut RankTree<ZKey>) {
        let max = cur.iter_rev().next().expect("non-empty").clone();
        self.segs.push(Arc::new(core::mem::take(cur)));
        self.maxes.push(max);
    }

    /// [`crate::Value::weight`]'s SegZSet arm — the flat ZSet model
    /// (member slots + ×2 heap bytes + rank-tree slots) plus the shells.
    pub(crate) fn weight_as_zset(&self) -> u64 {
        self.by_member.weight_shell_only()
            + self.by_member.keys().map(|m| 2 * m.heap_bytes() as u64).sum::<u64>()
            + (self.len() as u64).saturating_mul(crate::value::RANKTREE_SLOT_BYTES)
            + (self.segs.len() as u64).saturating_mul(8)
    }

    /// Every bucket AND every segment tree unique — the bio-drop gate.
    pub(crate) fn all_unique(&self) -> bool {
        self.by_member.all_unique() && self.segs.iter().all(|t| Arc::strong_count(t) == 1)
    }

    /// Test-only: `(strong_count, len)` per segment tree.
    #[cfg(test)]
    pub(crate) fn seg_stats(&self) -> Vec<(usize, usize)> {
        self.segs.iter().map(|t| (Arc::strong_count(t), t.len())).collect()
    }
}
