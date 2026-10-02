//! Building from entries already in order, as a promotion does: leaves
//! filled to [`LEAF_FILL`] and inner nodes to [`INNER_FILL`], so the
//! inserts that follow find room before anything splits.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::sync::Arc;

use super::inner::{INNER_MIN, Inner, Sep};
use super::leaf::{LEAF_MIN, Leaf};
use super::{Node, ZIndex, score_key};
use crate::value::SmallBytes;

const LEAF_FILL: usize = 10;
const INNER_FILL: usize = 24;

/// A built node, the entries under it and its smallest key.
type Built = (Node, usize, Sep);

/// How many nodes `n` items make at `fill` each, every node keeping at
/// least `min` when there is more than one.
fn groups(n: usize, fill: usize, min: usize) -> usize {
    let g = n.div_ceil(fill);
    if g >= 2 && n / g < min { g - 1 } else { g }
}

/// The `i`-th of `g` near-equal shares of `n`.
fn share(n: usize, g: usize, i: usize) -> usize {
    n / g + usize::from(i < n % g)
}

impl ZIndex {
    /// The index of the `n` `entries`, which come ascending and distinct.
    pub(crate) fn from_sorted(n: usize, entries: impl Iterator<Item = (f64, SmallBytes)>) -> Self {
        let leaves = groups(n, LEAF_FILL, LEAF_MIN);
        let mut entries = entries;
        let mut level: Vec<Built> = Vec::with_capacity(leaves);
        for g in 0..leaves {
            let mut l = Leaf::new();
            entries.by_ref().take(share(n, leaves, g)).for_each(|(sc, m)| l.push(score_key(sc), m));
            if l.len() > 0 {
                let low = (l.score(0), l.member(0).clone());
                level.push((Node::Leaf(Arc::new(l)), share(n, leaves, g), low));
            }
        }
        while level.len() > 1 {
            level = up(level);
        }
        level.pop().map_or_else(Self::default, |(root, len, _)| Self { root: Some(root), len })
    }
}

/// The inner level over `level`.
fn up(level: Vec<Built>) -> Vec<Built> {
    let n = level.len();
    let g = groups(n, INNER_FILL, INNER_MIN);
    let mut out = Vec::with_capacity(g);
    let mut kids = level.into_iter();
    for i in 0..g {
        let mut group = kids.by_ref().take(share(n, g, i));
        let Some((first, count, low)) = group.next() else { break };
        let mut node = Inner::single(first, count);
        group.for_each(|(kid, count, sep)| node.push_back(sep, kid, count));
        let total = node.total();
        out.push((Node::Inner(Arc::new(node)), total, low));
    }
    out
}
