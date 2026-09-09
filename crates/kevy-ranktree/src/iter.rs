//! In-order iterators, forward and reverse, both constructible at an
//! arbitrary rank with one O(log N) descent (the ZRANGE "start at rank S"
//! primitive — no skip-walk).
//!
//! A stack entry `(node, i)` means "`node.keys[i]` is the next key this node
//! owes". For the forward iterator everything left of `keys[i]` is already
//! yielded; for the reverse iterator everything right of it is.

use alloc::vec::Vec;

use crate::node::Node;

/// Forward (ascending) iterator. Created by [`crate::RankTree::iter`],
/// [`crate::RankTree::iter_from`] or [`crate::RankTree::range`].
/// # Examples
///
/// ```
/// let mut t = kevy_ranktree::RankTree::new();
/// for k in [3u32, 1, 2] { t.insert(k); }
/// // Ascending, and lazy: the tree is not copied to iterate it.
/// assert_eq!(t.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
/// ```
#[derive(Debug)]
pub struct Iter<'a, K> {
    stack: Vec<(&'a Node<K>, usize)>,
    remaining: usize,
}

impl<'a, K> Iter<'a, K> {
    /// Position the stack on the key at ascending `rank`; yields nothing
    /// when `rank` is past the end.
    pub(crate) fn new_from(root: &'a Node<K>, rank: usize) -> Self {
        let remaining = root.total.saturating_sub(rank);
        let mut it = Iter { stack: Vec::new(), remaining };
        if remaining == 0 {
            return it;
        }
        // Forward: resume at `keys[i]` of each node passed through, so
        // the separator to the RIGHT of the child taken. The last child
        // has no such separator, hence the bound.
        let mut stack = Vec::new();
        let (node, idx) = crate::node::descend_to_rank(root, rank, |n, i| {
            if i < n.keys.len() {
                stack.push((n, i));
            }
        });
        stack.push((node, idx));
        it.stack = stack;
        it
    }

    /// Stop after at most `cap` keys (the range iterator's upper bound).
    pub(crate) fn capped(mut self, cap: usize) -> Self {
        self.remaining = self.remaining.min(cap);
        self
    }

    /// After yielding `keys[i]` of `node`, everything up to the next key of
    /// `node` lives leftmost in `children[i + 1]`'s subtree.
    fn descend_left(&mut self, mut node: &'a Node<K>) {
        loop {
            self.stack.push((node, 0));
            if node.is_leaf() {
                return;
            }
            node = &node.children[0];
        }
    }
}

impl<'a, K> Iterator for Iter<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<&'a K> {
        if self.remaining == 0 {
            return None;
        }
        let (node, i) = self.stack.pop()?;
        let key = &node.keys[i];
        if i + 1 < node.keys.len() {
            self.stack.push((node, i + 1));
        }
        if !node.is_leaf() {
            self.descend_left(&node.children[i + 1]);
        }
        self.remaining -= 1;
        Some(key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K> ExactSizeIterator for Iter<'_, K> {}

/// Reverse (descending) iterator. Created by [`crate::RankTree::iter_rev`]
/// or [`crate::RankTree::iter_rev_from`].
/// # Examples
///
/// ```
/// let mut t = kevy_ranktree::RankTree::new();
/// for k in [3u32, 1, 2] { t.insert(k); }
/// assert_eq!(t.iter_rev().copied().collect::<Vec<_>>(), vec![3, 2, 1]);
/// ```
#[derive(Debug)]
pub struct IterRev<'a, K> {
    stack: Vec<(&'a Node<K>, usize)>,
    remaining: usize,
}

impl<'a, K> IterRev<'a, K> {
    /// Iterate the first `through` keys in DESCENDING order — i.e. start at
    /// ascending rank `through - 1` and walk down to rank 0. `through`
    /// saturates at the population.
    pub(crate) fn new_through(root: &'a Node<K>, through: usize) -> Self {
        let remaining = through.min(root.total);
        let mut it = IterRev { stack: Vec::new(), remaining };
        if remaining == 0 {
            return it;
        }
        let rank = remaining - 1; // ascending rank of the first yield
        // Reverse: resume at the separator to the LEFT of the child
        // taken. The first child has none, hence the bound — the mirror
        // of the forward case, and the only thing that differs between
        // the two walks.
        let mut stack = Vec::new();
        let (node, idx) = crate::node::descend_to_rank(root, rank, |n, i| {
            if i > 0 {
                stack.push((n, i - 1));
            }
        });
        stack.push((node, idx));
        it.stack = stack;
        it
    }

    /// After yielding `keys[i]` of `node`, the next-smaller keys live
    /// rightmost in `children[i]`'s subtree.
    fn descend_right(&mut self, mut node: &'a Node<K>) {
        loop {
            self.stack.push((node, node.keys.len() - 1));
            if node.is_leaf() {
                return;
            }
            node = &node.children[node.children.len() - 1];
        }
    }
}

impl<'a, K> Iterator for IterRev<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<&'a K> {
        if self.remaining == 0 {
            return None;
        }
        let (node, i) = self.stack.pop()?;
        let key = &node.keys[i];
        if i > 0 {
            self.stack.push((node, i - 1));
        }
        if !node.is_leaf() {
            self.descend_right(&node.children[i]);
        }
        self.remaining -= 1;
        Some(key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K> ExactSizeIterator for IterRev<'_, K> {}
