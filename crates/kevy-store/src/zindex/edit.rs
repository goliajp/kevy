//! Insert and remove: one descent each, every node on the path made
//! unique first, so a shared tree is copied along the path and nowhere
//! else.

use alloc::sync::Arc;

use super::inner::{INNER_CAP, Inner, Sep};
use super::leaf::{LEAF_CAP, Leaf};
use super::{Node, ZIndex};
use crate::value::SmallBytes;

/// A node that split: the separator before its new right half, the half,
/// and how many entries went with it.
struct Split {
    sep: Sep,
    node: Node,
    count: usize,
}

enum Added {
    Present,
    Fits,
    Split(Split),
}

impl ZIndex {
    /// Add `(sk, member)`; `false`, changing nothing, if it is there.
    pub(crate) fn insert_key(&mut self, sk: u64, member: SmallBytes) -> bool {
        let root = self.root.get_or_insert_with(|| Node::Leaf(Arc::new(Leaf::new())));
        match insert_rec(root, sk, member) {
            Added::Present => return false,
            Added::Fits => {}
            Added::Split(s) => {
                let left = root.clone();
                let left_count = self.len + 1 - s.count;
                *root = Node::Inner(Arc::new(Inner::pair(
                    (left, left_count),
                    s.sep,
                    (s.node, s.count),
                )));
            }
        }
        self.len += 1;
        true
    }

    /// Remove `(sk, member)`; whether it was there.
    pub(crate) fn remove_key(&mut self, sk: u64, member: &[u8]) -> bool {
        let Some(root) = self.root.as_mut() else { return false };
        if !remove_rec(root, sk, member) {
            return false;
        }
        self.len -= 1;
        let collapse = match root {
            Node::Inner(n) if n.len() == 1 => Some(Some(n.kid(0).clone())),
            Node::Leaf(l) if l.len() == 0 => Some(None),
            _ => None,
        };
        if let Some(next) = collapse {
            self.root = next;
        }
        true
    }
}

fn insert_rec(node: &mut Node, sk: u64, member: SmallBytes) -> Added {
    match node {
        Node::Leaf(l) => insert_leaf(Arc::make_mut(l), sk, member),
        Node::Inner(n) => {
            let n = Arc::make_mut(n);
            let i = n.route(sk, member.as_slice());
            let split = match insert_rec(n.kid_mut(i), sk, member) {
                Added::Split(s) => s,
                other => {
                    if matches!(other, Added::Fits) {
                        *n.count_mut(i) += 1;
                    }
                    return other;
                }
            };
            *n.count_mut(i) += 1;
            *n.count_mut(i) -= split.count;
            if n.len() < INNER_CAP {
                n.insert_after(i, split.sep, split.node, split.count);
                return Added::Fits;
            }
            let (up, mut right) = n.split_off(INNER_CAP / 2);
            if i < INNER_CAP / 2 {
                n.insert_after(i, split.sep, split.node, split.count);
            } else {
                right.insert_after(i - INNER_CAP / 2, split.sep, split.node, split.count);
            }
            let count = right.total();
            Added::Split(Split { sep: up, node: Node::Inner(Arc::new(right)), count })
        }
    }
}

fn insert_leaf(l: &mut Leaf, sk: u64, member: SmallBytes) -> Added {
    let Err(p) = l.search(sk, member.as_slice()) else { return Added::Present };
    if l.len() < LEAF_CAP {
        l.insert_at(p, sk, member);
        return Added::Fits;
    }
    let half = LEAF_CAP / 2 + 1;
    let mut right = l.split_off(half);
    if p <= half {
        l.insert_at(p, sk, member);
    } else {
        right.insert_at(p - half, sk, member);
    }
    let sep = (right.score(0), right.member(0).clone());
    let count = right.len();
    Added::Split(Split { sep, node: Node::Leaf(Arc::new(right)), count })
}

fn remove_rec(node: &mut Node, sk: u64, member: &[u8]) -> bool {
    match node {
        Node::Leaf(l) => {
            let l = Arc::make_mut(l);
            l.search(sk, member).map(|p| l.remove_at(p)).is_ok()
        }
        Node::Inner(n) => {
            let n = Arc::make_mut(n);
            let i = n.route(sk, member);
            if !remove_rec(n.kid_mut(i), sk, member) {
                return false;
            }
            *n.count_mut(i) -= 1;
            super::rebalance::after_removal(n, i);
            true
        }
    }
}
