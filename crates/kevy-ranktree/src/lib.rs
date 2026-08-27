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
#[derive(Clone)]
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
    #[must_use]
    pub fn new() -> Self {
        RankTree { root: Node::leaf() }
    }

    /// Number of keys in the tree. O(1): the root carries its subtree count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.root.total
    }

    /// Whether the tree holds no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.root.total == 0
    }

    /// Drop every key.
    pub fn clear(&mut self) {
        self.root = Node::leaf();
    }

    /// The key at ascending `rank` (0-based; rank 0 = smallest), or `None`
    /// past the end. O(log N): each level subtracts the child counts to the
    /// left instead of walking them.
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
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K> {
        self.iter_from(0)
    }

    /// Forward in-order iterator starting at ascending `rank` (seek is one
    /// O(log N) descent; an out-of-range rank yields nothing).
    #[must_use]
    pub fn iter_from(&self, rank: usize) -> Iter<'_, K> {
        Iter::new_from(&self.root, rank)
    }

    /// Reverse in-order iterator over all keys (largest first).
    #[must_use]
    pub fn iter_rev(&self) -> IterRev<'_, K> {
        IterRev::new_through(&self.root, self.root.total)
    }

    /// Reverse iterator that starts at ascending rank `rank - 1` and walks
    /// down to rank 0 — i.e. the largest `rank` keys, descending. `rank`
    /// saturates at `len()`.
    #[must_use]
    pub fn iter_rev_from(&self, rank: usize) -> IterRev<'_, K> {
        IterRev::new_through(&self.root, rank)
    }
}

impl<K: Ord> RankTree<K> {
    /// The ascending rank of `key`, or `None` if absent. O(log N).
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
    #[must_use]
    pub fn count_in<R: RangeBounds<K>>(&self, bounds: &R) -> usize {
        let (lo, hi) = self.bound_ranks(bounds);
        hi.saturating_sub(lo)
    }

    /// Iterate the keys inside `bounds` in ascending order: one O(log N)
    /// seek to the lower bound, then O(1) amortised per yielded key.
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
