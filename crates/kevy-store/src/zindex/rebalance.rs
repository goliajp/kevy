//! After a removal, a child that fell below half full takes one entry
//! (or child) from a neighbour, or merges with it when the two fit in
//! one node. Fill is what bounds the height; order, counts and every
//! search hold either way, so a pair this cannot pair up (never two
//! siblings of different heights, which every split and merge keeps
//! level) is left as it is.

use alloc::sync::Arc;

use super::Node;
use super::inner::{INNER_CAP, INNER_MIN, Inner};
use super::leaf::{LEAF_CAP, LEAF_MIN};

/// `(entries or children, the fewest it should keep, the most it holds)`.
fn fill(node: &Node) -> (usize, usize, usize) {
    match node {
        Node::Leaf(l) => (l.len(), LEAF_MIN, LEAF_CAP),
        Node::Inner(k) => (k.len(), INNER_MIN, INNER_CAP),
    }
}

/// Child `i` of `n` just lost an entry.
pub(super) fn after_removal(n: &mut Inner, i: usize) {
    let (len, min, cap) = fill(n.kid(i));
    if len >= min || n.len() < 2 {
        return;
    }
    let a = i.saturating_sub(1);
    let (left, right) = (fill(n.kid(a)).0, fill(n.kid(a + 1)).0);
    if left + right <= cap {
        merge(n, a);
    } else if i == a {
        take_from_right(n, a);
    } else {
        take_from_left(n, a);
    }
}

/// Child `a + 1` folds into child `a`.
fn merge(n: &mut Inner, a: usize) {
    let level = matches!(
        (n.kid(a), n.kid(a + 1)),
        (Node::Leaf(_), Node::Leaf(_)) | (Node::Inner(_), Node::Inner(_))
    );
    if !level {
        return;
    }
    let (sep, right, count) = n.remove(a + 1);
    *n.count_mut(a) += count;
    match (n.kid_mut(a), right) {
        (Node::Leaf(l), Node::Leaf(r)) => Arc::make_mut(l).append(Arc::unwrap_or_clone(r)),
        (Node::Inner(l), Node::Inner(r)) => Arc::make_mut(l).append(sep, Arc::unwrap_or_clone(r)),
        _ => {}
    }
}

/// Child `a` takes the smallest entry (or child) of child `a + 1`.
fn take_from_right(n: &mut Inner, a: usize) {
    let between = n.sep(a);
    let (l, r) = n.kid_pair(a);
    let (sep, moved) = match (l, r) {
        (Node::Leaf(l), Node::Leaf(r)) => {
            let (l, r) = (Arc::make_mut(l), Arc::make_mut(r));
            let (sk, m) = r.remove_at(0);
            l.push(sk, m);
            ((r.score(0), r.member(0).clone()), 1)
        }
        (Node::Inner(l), Node::Inner(r)) => {
            let (l, r) = (Arc::make_mut(l), Arc::make_mut(r));
            let (first_sep, kid, count) = r.pop_front();
            l.push_back(between, kid, count);
            (first_sep, count)
        }
        _ => return,
    };
    n.set_sep(a, sep);
    *n.count_mut(a) += moved;
    *n.count_mut(a + 1) -= moved;
}

/// Child `a + 1` takes the largest entry (or child) of child `a`.
fn take_from_left(n: &mut Inner, a: usize) {
    let between = n.sep(a);
    let (l, r) = n.kid_pair(a);
    let (sep, moved) = match (l, r) {
        (Node::Leaf(l), Node::Leaf(r)) => {
            let (l, r) = (Arc::make_mut(l), Arc::make_mut(r));
            let (sk, m) = l.remove_at(l.len() - 1);
            r.insert_at(0, sk, m.clone());
            ((sk, m), 1)
        }
        (Node::Inner(l), Node::Inner(r)) => {
            let (l, r) = (Arc::make_mut(l), Arc::make_mut(r));
            let (last_sep, kid, count) = l.pop_back();
            r.push_front(between, kid, count);
            (last_sep, count)
        }
        _ => return,
    };
    n.set_sep(a, sep);
    *n.count_mut(a) -= moved;
    *n.count_mut(a + 1) += moved;
}
