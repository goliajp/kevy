//! Reads: rank, partition by score, and ascending iteration from a rank —
//! each one descent, summing the counts of the children passed on the
//! left.

use super::inner::Inner;
use super::leaf::Leaf;
use super::{Node, ZIndex, key_score};

/// Inner levels an iterator can stand in. A level is added only when the
/// root splits, which takes at least 16 times the splits the level below
/// took: 20 levels would take more than 2^76 leaf splits.
const MAX_DEPTH: usize = 20;

impl ZIndex {
    /// The ascending rank of `(sk, member)`, if it is there.
    pub(crate) fn rank_of_key(&self, sk: u64, member: &[u8]) -> Option<usize> {
        let mut node = self.root.as_ref()?;
        let mut acc = 0;
        loop {
            match node {
                Node::Inner(n) => {
                    let i = n.route(sk, member);
                    acc += n.before(i);
                    node = n.kid(i);
                }
                Node::Leaf(l) => return l.search(sk, member).ok().map(|p| acc + p),
            }
        }
    }

    /// How many leading entries have a score `pred` holds for, where it
    /// holds on a prefix.
    pub(crate) fn partition(&self, pred: impl Fn(f64) -> bool) -> usize {
        let pred = |k: u64| pred(key_score(k));
        let Some(mut node) = self.root.as_ref() else { return 0 };
        let mut acc = 0;
        loop {
            match node {
                Node::Inner(n) => {
                    let j = n.partition(&pred);
                    acc += n.before(j);
                    node = n.kid(j);
                }
                Node::Leaf(l) => return acc + l.partition(&pred),
            }
        }
    }

    /// `(member, score)` in ascending order from `rank` on.
    pub(crate) fn iter_from(&self, mut rank: usize) -> Iter<'_> {
        let mut it = Iter { path: [None; MAX_DEPTH], depth: 0, leaf: None, pos: 0 };
        let Some(mut node) = self.root.as_ref().filter(|_| rank < self.len) else { return it };
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
                    it.pos = rank;
                    return it;
                }
            }
        }
    }
}

/// Ascending `(member, score)` pairs; see [`ZIndex::iter_from`].
#[derive(Debug)]
pub(crate) struct Iter<'a> {
    path: [Option<(&'a Inner, usize)>; MAX_DEPTH],
    depth: usize,
    leaf: Option<&'a Leaf>,
    pos: usize,
}

impl<'a> Iter<'a> {
    fn enter(&mut self, n: &'a Inner, i: usize) {
        self.path[self.depth] = Some((n, i));
        self.depth += 1;
    }

    /// From the leaf just finished to the next one, if any.
    fn next_leaf(&mut self) -> Option<&'a Leaf> {
        let mut node = loop {
            let (n, i) = self.path[self.depth.checked_sub(1)?]?;
            if i + 1 < n.len() {
                self.path[self.depth - 1] = Some((n, i + 1));
                break n.kid(i + 1);
            }
            self.depth -= 1;
        };
        loop {
            match node {
                Node::Inner(n) => {
                    self.enter(n, 0);
                    node = n.kid(0);
                }
                Node::Leaf(l) => return Some(l),
            }
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], f64);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let l = self.leaf?;
            if self.pos < l.len() {
                let i = self.pos;
                self.pos += 1;
                return Some((l.member(i).as_slice(), key_score(l.score(i))));
            }
            self.leaf = self.next_leaf();
            self.pos = 0;
        }
    }
}
