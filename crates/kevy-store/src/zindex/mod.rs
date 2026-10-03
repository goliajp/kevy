//! The score order of a big sorted set: one B+tree counted for rank,
//! whose nodes are shared `Arc`s.
//!
//! A write makes each node on its path unique (`Arc::make_mut`): with no
//! snapshot alive that is a check, with one it copies the path — a few
//! nodes, never more of the order. Every inner node counts the entries
//! under each child, so rank, partition by score and the start of an
//! iteration are each one descent.
//!
//! Scores are kept as [`score_key`]s, `u64`s that order as
//! `f64::total_cmp` orders scores: a search compares integers and reads a
//! member only where two scores are equal.

mod build;
mod edit;
mod inner;
mod leaf;
mod query;
mod rebalance;
mod rev;
#[cfg(test)]
mod tests;

use alloc::sync::Arc;

use crate::value::SmallBytes;
use inner::Inner;
use leaf::Leaf;

pub(crate) use query::Iter;
pub(crate) use rev::IterRev;

/// A child or the root: each kind in an allocation of its own size.
#[derive(Clone, Debug)]
pub(crate) enum Node {
    Leaf(Arc<Leaf>),
    Inner(Arc<Inner>),
}

/// Every `(score, member)` of a sorted set, ascending.
#[derive(Clone, Debug, Default)]
pub(crate) struct ZIndex {
    root: Option<Node>,
    len: usize,
}

/// `score` as a `u64` that orders as [`f64::total_cmp`] orders scores:
/// negative values complemented, the others with the top bit set.
pub(crate) fn score_key(score: f64) -> u64 {
    let b = score.to_bits();
    if b >> 63 == 1 { !b } else { b | 1 << 63 }
}

/// The score a [`score_key`] came from.
pub(crate) fn key_score(k: u64) -> f64 {
    f64::from_bits(if k >> 63 == 1 { k & !(1 << 63) } else { !k })
}

impl ZIndex {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Add `(score, member)`; `false`, changing nothing, if it is there.
    pub(crate) fn insert(&mut self, score: f64, member: SmallBytes) -> bool {
        self.insert_key(score_key(score), member)
    }

    /// Remove `(score, member)`; whether it was there.
    pub(crate) fn remove(&mut self, score: f64, member: &[u8]) -> bool {
        self.remove_key(score_key(score), member)
    }

    /// The ascending rank of `(score, member)`, if it is there.
    pub(crate) fn rank_of(&self, score: f64, member: &[u8]) -> Option<usize> {
        self.rank_of_key(score_key(score), member)
    }

    pub(crate) fn iter(&self) -> Iter<'_> {
        self.iter_from(0)
    }

    /// Whether no snapshot shares the root. The nodes below may still be
    /// shared with one that copied its path away; dropping the tree then
    /// only counts those down.
    pub(crate) fn root_unique(&self) -> bool {
        match &self.root {
            None => true,
            Some(Node::Leaf(l)) => Arc::strong_count(l) == 1,
            Some(Node::Inner(n)) => Arc::strong_count(n) == 1,
        }
    }
}
