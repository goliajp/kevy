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
pub struct Iter<'a, K> {
    stack: Vec<(&'a Node<K>, usize)>,
    remaining: usize,
}

impl<'a, K> Iter<'a, K> {
    /// Position the stack on the key at ascending `rank`; yields nothing
    /// when `rank` is past the end.
    pub(crate) fn new_from(root: &'a Node<K>, mut rank: usize) -> Self {
        let remaining = root.total.saturating_sub(rank);
        let mut it = Iter { stack: Vec::new(), remaining };
        if remaining == 0 {
            return it;
        }
        let mut node = root;
        'descent: loop {
            if node.is_leaf() {
                it.stack.push((node, rank));
                return it;
            }
            let mut i = 0;
            loop {
                let below = node.children[i].total;
                if rank < below {
                    if i < node.keys.len() {
                        it.stack.push((node, i));
                    }
                    node = &node.children[i];
                    continue 'descent;
                }
                rank -= below;
                if rank == 0 {
                    it.stack.push((node, i));
                    return it;
                }
                rank -= 1;
                i += 1;
            }
        }
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
        let mut rank = remaining - 1; // ascending rank of the first yield
        let mut node = root;
        'descent: loop {
            if node.is_leaf() {
                it.stack.push((node, rank));
                return it;
            }
            let mut i = 0;
            loop {
                let below = node.children[i].total;
                if rank < below {
                    if i > 0 {
                        it.stack.push((node, i - 1));
                    }
                    node = &node.children[i];
                    continue 'descent;
                }
                rank -= below;
                if rank == 0 {
                    it.stack.push((node, i));
                    return it;
                }
                rank -= 1;
                i += 1;
            }
        }
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
