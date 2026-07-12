//! Deletion, CLRS top-down style: before descending into a minimal child,
//! fatten it by borrowing from a sibling or merging with one, so the actual
//! removal never has to unwind an underflow back up the tree.

use crate::RankTree;
use crate::node::{MIN_KEYS, Node};

impl<K: Ord> RankTree<K> {
    /// Remove `key`; returns `false` (and changes nothing) when it is not
    /// present. O(log N).
    pub fn remove(&mut self, key: &K) -> bool {
        let removed = remove_rec(&mut self.root, key);
        if self.root.keys.is_empty() && !self.root.is_leaf() {
            // The root emptied into its single remaining child: shrink one
            // level so the height matches the population again.
            self.root = self.root.children.pop().expect("single child");
        }
        removed
    }
}

fn remove_rec<K: Ord>(node: &mut Node<K>, key: &K) -> bool {
    match node.keys.binary_search(key) {
        Ok(i) if node.is_leaf() => {
            node.keys.remove(i);
            node.total -= 1;
            true
        }
        Ok(i) => remove_hit_in_internal(node, key, i),
        Err(_) if node.is_leaf() => false,
        Err(i) => {
            let i = fatten_child(node, i);
            let removed = remove_rec(&mut node.children[i], key);
            if removed {
                node.total -= 1;
            }
            removed
        }
    }
}

/// The key to delete sits at `keys[i]` of an internal node. Replace it with
/// its in-order neighbour when a flanking child can spare a key; otherwise
/// merge the flanks and recurse into the merged child.
fn remove_hit_in_internal<K: Ord>(node: &mut Node<K>, key: &K, i: usize) -> bool {
    if node.children[i].keys.len() > MIN_KEYS {
        let pred = remove_max(&mut node.children[i]);
        node.keys[i] = pred;
    } else if node.children[i + 1].keys.len() > MIN_KEYS {
        let succ = remove_min(&mut node.children[i + 1]);
        node.keys[i] = succ;
    } else {
        // Both flanks minimal: fold keys[i] + right flank into the left
        // flank, then delete the key from inside the merged child.
        merge_children(node, i);
        let removed = remove_rec(&mut node.children[i], key);
        debug_assert!(removed, "merged child must contain the separator key");
    }
    node.total -= 1;
    true
}

/// Detach and return the largest key of `node`'s subtree.
fn remove_max<K: Ord>(node: &mut Node<K>) -> K {
    node.total -= 1;
    if node.is_leaf() {
        return node.keys.pop().expect("non-empty by fatten invariant");
    }
    let i = fatten_child(node, node.children.len() - 1);
    remove_max(&mut node.children[i])
}

/// Detach and return the smallest key of `node`'s subtree.
fn remove_min<K: Ord>(node: &mut Node<K>) -> K {
    node.total -= 1;
    if node.is_leaf() {
        return node.keys.remove(0);
    }
    let i = fatten_child(node, 0);
    remove_min(&mut node.children[i])
}

/// Guarantee `children[i]` has > [`MIN_KEYS`] keys before descending into it,
/// by borrowing through the parent from a rich sibling, or merging with a
/// minimal one. Returns the (possibly shifted) index to descend into.
fn fatten_child<K: Ord>(node: &mut Node<K>, i: usize) -> usize {
    if node.children[i].keys.len() > MIN_KEYS {
        return i;
    }
    if i > 0 && node.children[i - 1].keys.len() > MIN_KEYS {
        borrow_from_left(node, i);
        i
    } else if i + 1 < node.children.len() && node.children[i + 1].keys.len() > MIN_KEYS {
        borrow_from_right(node, i);
        i
    } else if i > 0 {
        merge_children(node, i - 1);
        i - 1
    } else {
        merge_children(node, i);
        i
    }
}

/// Rotate right through the separator: the left sibling's last key moves up
/// to `keys[i-1]`, the old separator moves down to the front of
/// `children[i]` (bringing the sibling's last subtree along, if any).
fn borrow_from_left<K>(node: &mut Node<K>, i: usize) {
    let (up, moved) = {
        let left = &mut node.children[i - 1];
        let up = left.keys.pop().expect("left sibling is rich");
        let moved = left.children.pop(); // None exactly when it is a leaf
        left.recount();
        (up, moved)
    };
    let down = core::mem::replace(&mut node.keys[i - 1], up);
    let child = &mut node.children[i];
    child.keys.insert(0, down);
    if let Some(sub) = moved {
        child.children.insert(0, sub);
    }
    child.recount();
}

/// Rotate left through the separator: mirror of [`borrow_from_left`].
fn borrow_from_right<K>(node: &mut Node<K>, i: usize) {
    let (up, moved) = {
        let right = &mut node.children[i + 1];
        let up = right.keys.remove(0);
        let moved =
            if right.children.is_empty() { None } else { Some(right.children.remove(0)) };
        right.recount();
        (up, moved)
    };
    let down = core::mem::replace(&mut node.keys[i], up);
    let child = &mut node.children[i];
    child.keys.push(down);
    if let Some(sub) = moved {
        child.children.push(sub);
    }
    child.recount();
}

/// Fold `keys[i]` and `children[i+1]` into `children[i]`. The parent's
/// `total` is unchanged — every key stays inside its subtree.
fn merge_children<K>(node: &mut Node<K>, i: usize) {
    let separator = node.keys.remove(i);
    let right = node.children.remove(i + 1);
    let left = &mut node.children[i];
    left.keys.push(separator);
    left.keys.extend(right.keys);
    left.children.extend(right.children);
    left.recount();
}
