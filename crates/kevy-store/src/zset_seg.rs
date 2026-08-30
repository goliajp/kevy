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
//! `.claude/rfcs/`.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::seg_map::SegMap;
use crate::value::{Score, ScoreBound, SmallBytes};
use alloc::sync::Arc;
use kevy_ranktree::RankTree;

/// Flat `Value::ZSet` size at which a write promotes to the segmented
/// representation.
pub const Z_PROMOTE: usize = 16 * 1024;
/// Entries per score-ordered segment tree; one segment is the per-write
/// COW clone bound, and — as with `seg_map::BUCKET_SPLIT` — the grain
/// score-scattered write bursts aggregate over per tick. 2K entries
/// keeps a burst's per-tick clone total under the tick bar (empirically
/// sized alongside `BUCKET_SPLIT` — see its note).
pub const ZSEG_CAP: usize = 512;

type ZKey = (Score, SmallBytes);

/// A giant sorted set: sharded member→score map + ordered segment
/// trees. Segments are non-empty and range-disjoint; `maxes[i]` caches
/// `segs[i]`'s largest key for O(log segments) routing.
#[derive(Clone, Default)]
pub struct SegZSetData {
    by_member: SegMap<f64>,
    segs: Vec<Arc<RankTree<ZKey>>>,
    maxes: Vec<ZKey>,
}

impl SegZSetData {
    #[inline]
    /// Members across every segment, as a running count.
    pub fn len(&self) -> usize {
        self.by_member.len()
    }

    #[inline]
    /// Whether the sorted set holds no members.
    pub fn is_empty(&self) -> bool {
        self.by_member.is_empty()
    }

    /// One member's score, or `None` if it is not present. A lookup
    /// through the member index, not a walk of the score order.
    pub fn score_of(&self, member: &[u8]) -> Option<f64> {
        self.by_member.get(member).copied()
    }

    /// Membership, on the same index path as `score_of` and without
    /// reading the score.
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
    pub fn ordered(&self) -> impl Iterator<Item = (&[u8], f64)> {
        self.segs.iter().flat_map(|t| t.iter()).map(|(s, m)| (m.as_slice(), s.0))
    }

    /// Like [`Self::ordered`] but starting at ascending `rank` — an
    /// O(segments) prefix walk, then a seek inside the hit segment.
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
    pub fn score_start_rank(&self, min: &ScoreBound) -> usize {
        self.frontier_rank(|s| !min.ge_ok(s))
    }

    /// One past the last rank whose score satisfies `max` as an upper
    /// bound.
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
