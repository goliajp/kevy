//! Insertion: descend to the leaf, insert, and split overfull nodes on the
//! way back up (each split hands its median key to the parent).

use crate::RankTree;
use crate::node::{MAX_KEYS, Node};

impl<K: Ord> RankTree<K> {
    /// Insert `key`; returns `false` (and changes nothing) when an equal key
    /// is already present. O(log N).
    pub fn insert(&mut self, key: K) -> bool {
        let (inserted, split) = insert_rec(&mut self.root, key);
        if let Some((median, right)) = split {
            // The root split: grow the tree one level.
            let left = core::mem::replace(&mut self.root, Node::leaf());
            self.root.keys.push(median);
            self.root.children.push(left);
            self.root.children.push(right);
            self.root.recount();
        }
        inserted
    }
}

/// The `(median key, right half)` a split hands to the parent.
type Split<K> = Option<(K, Node<K>)>;

/// Recursive descent. Returns whether a key was inserted, plus the
/// `(median, right half)` a split of THIS node hands to its parent.
fn insert_rec<K: Ord>(node: &mut Node<K>, key: K) -> (bool, Split<K>) {
    let at = match node.keys.binary_search(&key) {
        Ok(_) => return (false, None), // duplicate — the set stays as-is
        Err(i) => i,
    };
    if node.is_leaf() {
        node.keys.insert(at, key);
        node.total += 1;
        return (true, split_if_overfull(node));
    }
    let (inserted, split) = insert_rec(&mut node.children[at], key);
    if inserted {
        node.total += 1;
    }
    if let Some((median, right)) = split {
        node.keys.insert(at, median);
        node.children.insert(at + 1, right);
        return (inserted, split_if_overfull(node));
    }
    (inserted, None)
}

/// When `node` exceeds [`MAX_KEYS`], carve off its upper half and pop the
/// median for the parent. Both halves keep ≥ [`crate::node::MIN_KEYS`] keys.
fn split_if_overfull<K>(node: &mut Node<K>) -> Split<K> {
    if node.keys.len() <= MAX_KEYS {
        return None;
    }
    let mid = node.keys.len() / 2;
    let right_keys = node.keys.split_off(mid + 1);
    let median = node.keys.pop().expect("split point exists");
    let right_children =
        if node.is_leaf() { alloc::vec::Vec::new() } else { node.children.split_off(mid + 1) };
    let mut right = Node { keys: right_keys, children: right_children, total: 0 };
    right.recount();
    node.recount();
    Some((median, right))
}
