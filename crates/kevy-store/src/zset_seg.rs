//! Segmented sorted set — element-granularity COW for giant zsets.
//!
//! A zset past [`Z_PROMOTE`] members stops being one `Arc<ZSetData>`
//! and becomes `Value::SegZSet`: the member→score side is a
//! [`SegMap`] (the Stage-HS bucket-sharded stone), and the
//! score-ordered side is one B+tree counted for rank whose nodes are
//! `Arc`-shared. A snapshot view pins everything with one outer Arc
//! clone; the first write during that window clones one member bucket
//! plus the tree nodes on its path — never the whole value. Rank,
//! partition by score and the start of an iteration are one descent
//! each, at any size.
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

use crate::seg_map::SegMap;
use crate::value::{Score, ScoreBound, SmallBytes};
use crate::zindex::ZIndex;

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
/// The most entries one score-ordered segment held, when the score order
/// was a row of segment trees. It is one tree now, copied a path at a
/// time; nothing reads this.
///
/// ```
/// # #![allow(deprecated)]
/// assert_eq!(kevy_store::zset_seg::ZSEG_CAP, 512);
/// ```
#[deprecated(since = "7.3.0", note = "the score order is one tree; nothing reads this")]
pub const ZSEG_CAP: usize = 512;

/// A giant sorted set: sharded member→score map + one rank-counted
/// score order.
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
    order: ZIndex,
}

impl SegZSetData {
    /// The member → score index, which a `ZSCAN` sweep pages through.
    pub(crate) fn by_member(&self) -> &SegMap<f64> {
        &self.by_member
    }

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
        self.insert_weighed(member, score).0
    }

    /// [`Self::insert`], also answering by how many bytes the member
    /// table grew; the score order is weighed per member.
    pub(crate) fn insert_weighed(&mut self, member: &[u8], score: f64) -> (bool, i64) {
        let smb = SmallBytes::from_slice(member);
        let (old, grown) = self.by_member.insert_sized(smb.clone(), score);
        if let Some(old_sc) = old {
            // See ZSetData::insert: an unchanged score leaves the order,
            // and under a live snapshot its path, alone.
            if Score(old_sc) == Score(score) {
                return (false, grown);
            }
            self.order.remove(old_sc, smb.as_slice());
        }
        self.order.insert(score, smb);
        (old.is_none(), grown)
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
        self.remove_weighed(member).is_some()
    }

    /// [`Self::remove`]; when the member was there, by how many bytes the
    /// structure around the members moved: none, the member table does
    /// not shrink and the score order is weighed per member.
    pub(crate) fn remove_weighed(&mut self, member: &[u8]) -> Option<i64> {
        let sc = self.by_member.remove(member)?;
        self.order.remove(sc, member);
        Some(0)
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
        self.order.iter()
    }

    /// Like [`Self::ordered`] but starting at ascending `rank`: one
    /// descent to it.
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
        self.order.iter_from(rank)
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
        self.order.rank_of(score, member)
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
    /// holds: one descent.
    fn frontier_rank<F: Fn(f64) -> bool>(&self, pred: F) -> usize {
        self.order.partition(pred)
    }

    /// Build from the flat representation: the order is built bottom up
    /// from the flat one's; members re-shard through the SegMap insert.
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
        let mut by_member = SegMap::default();
        for (m, sc) in flat.ordered() {
            by_member.insert(SmallBytes::from_slice(m), sc);
        }
        let entries = flat.ordered().map(|(m, sc)| (sc, SmallBytes::from_slice(m)));
        let order = ZIndex::from_sorted(by_member.len(), entries);
        SegZSetData { by_member, order }
    }

    /// [`crate::Value::weight`]'s SegZSet arm — the flat ZSet model
    /// (member slots + ×2 heap bytes + rank-tree slots) plus the shells.
    pub(crate) fn weight_as_zset(&self) -> u64 {
        crate::seg_map::arc_box::<Self>()
            + self.by_member.shell_bytes()
            + self.by_member.keys().map(|m| 2 * crate::hash_weight::held(m)).sum::<u64>()
            + (self.len() as u64).saturating_mul(crate::value::RANKTREE_SLOT_BYTES)
    }

    /// Every bucket unique and the score order's root unshared — the
    /// bio-drop gate. Nodes below a root a write copied may still be
    /// shared with a snapshot; the drop then only counts them down.
    pub(crate) fn all_unique(&self) -> bool {
        self.by_member.all_unique() && self.order.root_unique()
    }
}
