//! The B-tree node: a sorted key array, the child pointers, and the one
//! field that makes order statistics possible — `total`, the number of keys
//! in the whole subtree rooted here.

use alloc::vec::Vec;

/// Maximum keys per node. 15 keys ⇒ splits produce 8/7, both ≥ [`MIN_KEYS`],
/// and a node of 15 × pointer-sized-ish keys stays within a few cache lines.
pub(crate) const MAX_KEYS: usize = 15;
/// Minimum keys per non-root node (⌊15/2⌋ = 7, the classic B-tree half-full
/// invariant).
pub(crate) const MIN_KEYS: usize = MAX_KEYS / 2;

/// One node. `children` is empty exactly when the node is a leaf; an
/// internal node always has `keys.len() + 1` children.
#[derive(Clone)]
pub(crate) struct Node<K> {
    /// Sorted keys.
    pub(crate) keys: Vec<K>,
    /// Child subtrees (`keys.len() + 1` of them), or empty for a leaf.
    pub(crate) children: Vec<Node<K>>,
    /// Keys in this whole subtree: `keys.len()` + every child's `total`.
    /// The order-statistic augmentation — every rank/select/partition
    /// descent reads it instead of walking the subtree.
    pub(crate) total: usize,
}

impl<K> Node<K> {
    /// A fresh empty leaf.
    pub(crate) fn leaf() -> Self {
        Node { keys: Vec::new(), children: Vec::new(), total: 0 }
    }

    /// Leaf ⇔ no children.
    pub(crate) fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Recompute `total` from this node's keys and its children's (already
    /// correct) totals. O(children) — used after structural edits (split,
    /// borrow, merge) instead of threading deltas through every branch.
    pub(crate) fn recount(&mut self) {
        self.total = self.keys.len() + self.children.iter().map(|c| c.total).sum::<usize>();
    }
}
