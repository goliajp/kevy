//! Descending iteration and partition by a predicate on whole keys.

use super::inner::Inner;
use super::leaf::Leaf;
use super::{Node, ZIndex, key_score};

/// Inner levels a walk can stand in; see the forward iterator's bound.
const MAX_DEPTH: usize = 20;

impl ZIndex {
    /// How many leading entries `pred` holds for, where it holds on a
    /// prefix of the order — by member alone, for a range by bytes.
    pub(crate) fn partition_keys(&self, pred: impl Fn(f64, &[u8]) -> bool) -> usize {
        let pred = |k: u64, m: &[u8]| pred(key_score(k), m);
        let Some(mut node) = self.root.as_ref() else { return 0 };
        let mut acc = 0;
        loop {
            match node {
                Node::Inner(n) => {
                    let j = n.partition_keys(&pred);
                    acc += n.before(j);
                    node = n.kid(j);
                }
                Node::Leaf(l) => return acc + l.partition_keys(&pred),
            }
        }
    }

    /// `(member, score)` descending from ascending rank `through - 1` down
    /// to rank 0: the first `through` entries read backwards.
    pub(crate) fn iter_rev_through(&self, through: usize) -> IterRev<'_> {
        let mut it = IterRev { path: [None; MAX_DEPTH], depth: 0, leaf: None, pos: 0 };
        let through = through.min(self.len);
        let Some(mut node) = self.root.as_ref().filter(|_| through > 0) else { return it };
        let mut rank = through - 1;
        loop {
            match node {
                Node::Inner(n) => {
                    let mut i = 0;
                    while i + 1 < n.len() && rank >= n.count(i) {
                        rank -= n.count(i);
                        i += 1;
                    }
                    it.enter(n, i);
                    node = n.kid(i);
                }
                Node::Leaf(l) => {
                    it.leaf = Some(l);
                    it.pos = rank + 1;
                    return it;
                }
            }
        }
    }
}

/// Descending `(member, score)` pairs; see [`ZIndex::iter_rev_through`].
#[derive(Debug)]
pub(crate) struct IterRev<'a> {
    path: [Option<(&'a Inner, usize)>; MAX_DEPTH],
    depth: usize,
    leaf: Option<&'a Leaf>,
    /// One past the next entry to yield in `leaf`.
    pos: usize,
}

impl<'a> IterRev<'a> {
    fn enter(&mut self, n: &'a Inner, i: usize) {
        self.path[self.depth] = Some((n, i));
        self.depth += 1;
    }

    /// From the leaf just finished to the one before it, if any.
    fn prev_leaf(&mut self) -> Option<&'a Leaf> {
        let mut node = loop {
            let (n, i) = self.path[self.depth.checked_sub(1)?]?;
            if i > 0 {
                self.path[self.depth - 1] = Some((n, i - 1));
                break n.kid(i - 1);
            }
            self.depth -= 1;
        };
        loop {
            match node {
                Node::Inner(n) => {
                    self.enter(n, n.len() - 1);
                    node = n.kid(n.len() - 1);
                }
                Node::Leaf(l) => return Some(l),
            }
        }
    }
}

impl<'a> Iterator for IterRev<'a> {
    type Item = (&'a [u8], f64);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let l = self.leaf?;
            if self.pos > 0 {
                self.pos -= 1;
                return Some((l.member(self.pos).as_slice(), key_score(l.score(self.pos))));
            }
            self.leaf = self.prev_leaf();
            self.pos = self.leaf.map_or(0, Leaf::len);
        }
    }
}
