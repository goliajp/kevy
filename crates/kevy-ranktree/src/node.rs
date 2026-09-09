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
#[derive(Debug, Clone)]
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
    /// A fresh empty leaf, sized for the most it can ever hold.
    ///
    /// A node fills to at most `MAX_KEYS` keys before it splits, and one
    /// more transiently while splitting — so `MAX_KEYS + 1` is not a
    /// guess, it is the ceiling. Growing from empty instead walked the
    /// doubling ladder (0 → 4 → 8 → 16) and paid two reallocations and
    /// their memcpys per node on the way. Capacity ends at 16 either
    /// way, so this costs no memory.
    pub(crate) fn leaf() -> Self {
        Node { keys: Vec::with_capacity(MAX_KEYS + 1), children: Vec::new(), total: 0 }
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

/// Walk from `root` toward ascending `rank`, returning the node and the
/// index within its `keys` that the rank lands on.
///
/// `on_descend(node, i)` fires once per internal node the walk passes
/// through, with the index of the child taken. Callers that need the
/// path — the iterators, which keep a stack of where to resume — build
/// it there; `select`, which needs only the destination, passes a
/// closure that does nothing.
///
/// This existed three times: once in `select` and once in each
/// iterator's constructor, differing only in what they pushed. They are
/// the same walk, and they end the same way — every one of them uses
/// `(node, idx)` identically whether it stopped at a leaf offset or
/// landed exactly on a separator. `mod/no-second-implementation` is not
/// hypothetical here: `lib.rs` carries a note recording that one copy's
/// doc had come to say the opposite of what its code did, and that
/// writing an example is what caught it. Mirrored code diverges, and
/// both sides' tests stay green while it does.
pub(crate) fn descend_to_rank<'a, K>(
    root: &'a Node<K>,
    mut rank: usize,
    mut on_descend: impl FnMut(&'a Node<K>, usize),
) -> (&'a Node<K>, usize) {
    let mut node = root;
    loop {
        if node.is_leaf() {
            return (node, rank);
        }
        let mut i = 0;
        loop {
            let below = node.children[i].total;
            if rank < below {
                on_descend(node, i);
                node = &node.children[i];
                break;
            }
            rank -= below;
            if rank == 0 {
                return (node, i);
            }
            rank -= 1;
            i += 1;
        }
    }
}
