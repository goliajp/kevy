//! `kevy-ranktree` — an order-statistic ordered set: a hand-written B-tree
//! whose every node carries its subtree element count, so **rank** queries
//! ("what position is this key at?") and **select** queries ("what key is at
//! position k?") are single O(log N) descents instead of linear walks.
//!
//! This is the structure `std::collections::BTreeSet` cannot be: std's tree
//! is not augmentable, so `Iterator::position` over it is the only way to a
//! rank — O(N). Here each node's `total` field turns the same descent that
//! finds a key into the arithmetic that counts everything to its left.
//!
//! # Why a counted B-tree and not a spanned skiplist
//!
//! Redis solves the same problem with a skiplist whose forward pointers carry
//! spans. Both give O(log N) rank/select; the B-tree was chosen because:
//!
//! * **Cache behaviour** — a node holds up to [`node::MAX_KEYS`] keys in one
//!   contiguous buffer, so a descent touches ~log₈(N) cache lines where a
//!   skiplist chases one pointer per level *and* per element during range
//!   walks. Ordered full scans (persistence rewrites, snapshots) iterate
//!   arrays, not linked nodes.
//! * **No probabilistic balance** — a skiplist needs a random level
//!   generator and its worst case is only expected-O(log N); the B-tree's
//!   bounds are deterministic.
//! * **It matches what it replaces** — the callers previously held a
//!   `BTreeSet` and were tuned for its iteration pattern; this keeps that
//!   pattern and adds the rank arithmetic.
//!
//! The price is B-tree deletion (borrow/merge rebalancing), which is more
//! code than a skiplist unlink — it lives in `remove.rs` and is held down by
//! an exhaustive random-operation oracle test (`tests/oracle.rs`) that
//! replays thousands of insert/remove/rank/select/range operations against a
//! sorted-`Vec` reference.
//!
//! # Complexity
//!
//! | operation | cost |
//! |---|---|
//! | [`RankTree::insert`], [`RankTree::remove`] | O(log N) |
//! | [`RankTree::rank_of`], [`RankTree::select`] | O(log N) |
//! | [`RankTree::partition_point`], [`RankTree::count_in`] | O(log N) |
//! | [`RankTree::range`], [`RankTree::iter_from`] | O(log N) seek + O(1) amortised per item |
//! | [`RankTree::iter`], [`RankTree::iter_rev`] | O(1) amortised per item |
//!
//! Constraints: pure Rust, zero dependencies, `no_std`-capable behind the
//! `alloc` feature, `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod insert;
mod iter;
mod node;
mod remove;
#[cfg(test)]
mod tests_invariants;

use core::ops::{Bound, RangeBounds};

pub use iter::{Iter, IterRev};
use node::Node;

/// An ordered set of `K` with O(log N) order statistics (rank / select).
///
/// Duplicate keys are rejected ([`RankTree::insert`] returns `false`), which
/// makes every key's rank unique — the property the rank arithmetic rests on.
/// # Examples
///
/// The point of an order-statistic tree is that rank is a lookup, not a
/// scan: inserting out of order still answers rank in sorted order.
///
/// ```
/// let mut t = kevy_ranktree::RankTree::new();
/// for k in [30, 10, 20] {
///     assert!(t.insert(k), "each key is new");
/// }
/// assert_eq!(t.len(), 3);
/// assert_eq!(t.rank_of(&10), Some(0));
/// assert_eq!(t.rank_of(&20), Some(1));
/// assert_eq!(t.rank_of(&30), Some(2));
/// assert_eq!(t.rank_of(&25), None, "a key that is not there has no rank");
/// ```
///
/// Re-inserting an existing key reports that it was not new, and leaves
/// the tree alone.
///
/// ```
/// let mut t = kevy_ranktree::RankTree::new();
/// assert!(t.insert(1));
/// assert!(!t.insert(1));
/// assert_eq!(t.len(), 1);
/// ```
#[derive(Clone)]
pub struct RankTree<K> {
    root: Node<K>,
}

impl<K> Default for RankTree<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> RankTree<K> {
    /// An empty tree.
    /// # Examples
    ///
    /// ```
    /// let t: kevy_ranktree::RankTree<u32> = kevy_ranktree::RankTree::new();
    /// assert!(t.is_empty());
    /// assert_eq!(t.len(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        RankTree { root: Node::leaf() }
    }

    /// Number of keys in the tree. O(1): the root carries its subtree count.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// t.insert(1u32); t.insert(1);
    /// assert_eq!(t.len(), 1, "a set: the repeat did not count");
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.root.total
    }

    /// Whether the tree holds no keys.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// assert!(t.is_empty());
    /// t.insert(1u32);
    /// assert!(!t.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.root.total == 0
    }

    /// Drop every key.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [1u32, 2, 3] { t.insert(k); }
    /// t.clear();
    /// assert!(t.is_empty());
    /// assert_eq!(t.select(0), None);
    /// ```
    pub fn clear(&mut self) {
        self.root = Node::leaf();
    }

    /// The key at ascending `rank` (0-based; rank 0 = smallest), or `None`
    /// past the end. O(log N): each level subtracts the child counts to the
    /// left instead of walking them.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [30u32, 10, 20] { t.insert(k); }
    /// // Rank is 0-based over the SORTED order, not insertion order.
    /// assert_eq!(t.select(0), Some(&10));
    /// assert_eq!(t.select(2), Some(&30));
    /// assert_eq!(t.select(3), None);
    /// ```
    #[must_use]
    pub fn select(&self, mut rank: usize) -> Option<&K> {
        if rank >= self.root.total {
            return None;
        }
        let mut node = &self.root;
        loop {
            if node.is_leaf() {
                return Some(&node.keys[rank]);
            }
            let mut i = 0;
            loop {
                let below = node.children[i].total;
                if rank < below {
                    node = &node.children[i];
                    break;
                }
                rank -= below;
                if rank == 0 {
                    return Some(&node.keys[i]);
                }
                rank -= 1;
                i += 1;
            }
        }
    }

    /// Forward in-order iterator over all keys.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [30u32, 10, 20] { t.insert(k); }
    /// assert_eq!(t.iter().copied().collect::<Vec<_>>(), vec![10, 20, 30]);
    /// assert_eq!(t.iter_rev().copied().collect::<Vec<_>>(), vec![30, 20, 10]);
    /// ```
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K> {
        self.iter_from(0)
    }

    /// Forward in-order iterator starting at ascending `rank` (seek is one
    /// O(log N) descent; an out-of-range rank yields nothing).
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [10u32, 20, 30] { t.insert(k); }
    /// // Start at a RANK, so paging costs a descent rather than a skip.
    /// assert_eq!(t.iter_from(1).copied().collect::<Vec<_>>(), vec![20, 30]);
    /// assert!(t.iter_from(9).next().is_none());
    /// ```
    #[must_use]
    pub fn iter_from(&self, rank: usize) -> Iter<'_, K> {
        Iter::new_from(&self.root, rank)
    }

    /// Reverse in-order iterator over all keys (largest first).
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [10u32, 20, 30] { t.insert(k); }
    /// assert_eq!(t.iter_rev().copied().collect::<Vec<_>>(), vec![30, 20, 10]);
    /// ```
    #[must_use]
    pub fn iter_rev(&self) -> IterRev<'_, K> {
        IterRev::new_through(&self.root, self.root.total)
    }

    /// Reverse iterator that starts at ascending rank `rank - 1` and walks
    /// down to rank 0 — i.e. the SMALLEST `rank` keys, descending. `rank`
    /// saturates at `len()`.
    ///
    /// (This line used to say "the largest `rank` keys". It is the first
    /// `rank` keys read backwards, which is the opposite end; writing the
    /// example below is what caught it.)
    ///
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [10u32, 20, 30] { t.insert(k); }
    /// // Ascending ranks 1 and 0, walked down: the two SMALLEST keys.
    /// assert_eq!(t.iter_rev_from(2).copied().collect::<Vec<_>>(), vec![20, 10]);
    /// assert_eq!(t.iter_rev_from(1).copied().collect::<Vec<_>>(), vec![10]);
    /// assert!(t.iter_rev_from(0).next().is_none());
    /// // Saturating, not panicking.
    /// assert_eq!(t.iter_rev_from(99).copied().collect::<Vec<_>>(), vec![30, 20, 10]);
    /// ```
    #[must_use]
    pub fn iter_rev_from(&self, rank: usize) -> IterRev<'_, K> {
        IterRev::new_through(&self.root, rank)
    }
}

impl<K: Ord> RankTree<K> {
    /// The ascending rank of `key`, or `None` if absent. O(log N).
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [10u32, 20, 30] { t.insert(k); }
    /// assert_eq!(t.rank_of(&20), Some(1));
    /// // The inverse of `select`, and `None` for a key that is absent —
    /// // never the rank it WOULD have.
    /// assert_eq!(t.rank_of(&25), None);
    /// assert_eq!(t.select(t.rank_of(&30).unwrap()), Some(&30));
    /// ```
    #[must_use]
    pub fn rank_of(&self, key: &K) -> Option<usize> {
        let mut node = &self.root;
        let mut acc = 0usize;
        loop {
            match node.keys.binary_search(key) {
                Ok(i) => {
                    for c in node.children.iter().take(i + 1) {
                        acc += c.total;
                    }
                    return Some(acc + i);
                }
                Err(i) => {
                    if node.is_leaf() {
                        return None;
                    }
                    acc += i;
                    for c in &node.children[..i] {
                        acc += c.total;
                    }
                    node = &node.children[i];
                }
            }
        }
    }

    /// The number of leading keys for which `pred` holds. `pred` must be
    /// monotone (true for a prefix of the ordered keys, false after) — the
    /// same contract as `slice::partition_point`. O(log N).
    ///
    /// This is the primitive behind every bound query: a caller keying the
    /// tree by a composite like `(score, member)` can seek on the score
    /// alone with `pred = |(s, _)| *s < bound`.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [1u32, 3, 5, 7] { t.insert(k); }
    /// // The rank of the first key the predicate rejects — so the whole
    /// // tree when it never does, and 0 when it always does.
    /// assert_eq!(t.partition_point(|k| *k < 5), 2);
    /// assert_eq!(t.partition_point(|_| true), 4);
    /// assert_eq!(t.partition_point(|_| false), 0);
    /// ```
    #[must_use]
    pub fn partition_point<F: FnMut(&K) -> bool>(&self, mut pred: F) -> usize {
        let mut node = &self.root;
        let mut acc = 0usize;
        loop {
            let i = node.keys.partition_point(|k| pred(k));
            if node.is_leaf() {
                return acc + i;
            }
            // keys[..i] hold, so each of children[..i] (all strictly below
            // keys[i-1]) holds entirely; children[i] is the mixed frontier.
            acc += i;
            for c in &node.children[..i] {
                acc += c.total;
            }
            node = &node.children[i];
        }
    }

    /// How many keys fall inside `bounds`. Two [`RankTree::partition_point`]
    /// descents — O(log N), never a scan.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [1u32, 3, 5, 7] { t.insert(k); }
    /// // Counted from the subtree sizes, not by walking the range.
    /// assert_eq!(t.count_in(&(3..=7)), 3);
    /// assert_eq!(t.count_in(&(3..7)), 2);
    /// assert_eq!(t.count_in(&(8..)), 0);
    /// ```
    #[must_use]
    pub fn count_in<R: RangeBounds<K>>(&self, bounds: &R) -> usize {
        let (lo, hi) = self.bound_ranks(bounds);
        hi.saturating_sub(lo)
    }

    /// Iterate the keys inside `bounds` in ascending order: one O(log N)
    /// seek to the lower bound, then O(1) amortised per yielded key.
    /// # Examples
    ///
    /// ```
    /// let mut t = kevy_ranktree::RankTree::new();
    /// for k in [1u32, 3, 5, 7] { t.insert(k); }
    /// let got: Vec<_> = t.range(&(3..=5)).copied().collect();
    /// assert_eq!(got, vec![3, 5]);
    /// ```
    #[must_use]
    pub fn range<R: RangeBounds<K>>(&self, bounds: &R) -> Iter<'_, K> {
        let (lo, hi) = self.bound_ranks(bounds);
        Iter::new_from(&self.root, lo).capped(hi.saturating_sub(lo))
    }

    /// `(first rank inside, first rank past)` for `bounds`.
    fn bound_ranks<R: RangeBounds<K>>(&self, bounds: &R) -> (usize, usize) {
        let lo = match bounds.start_bound() {
            Bound::Unbounded => 0,
            Bound::Included(k) => self.partition_point(|x| x < k),
            Bound::Excluded(k) => self.partition_point(|x| x <= k),
        };
        let hi = match bounds.end_bound() {
            Bound::Unbounded => self.len(),
            Bound::Included(k) => self.partition_point(|x| x <= k),
            Bound::Excluded(k) => self.partition_point(|x| x < k),
        };
        (lo, hi)
    }
}
