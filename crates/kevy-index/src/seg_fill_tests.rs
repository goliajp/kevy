//! Leaf fill whatever order entries arrive and leave in.

use std::collections::BTreeSet;

use super::balance::FILL;
use super::tests::check;
use super::*;
use crate::seg_codec::{Codec, Form};
use crate::seg_leaf::{Ent, Leaf, Shape};
use crate::{IndexValue, Segment};

pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, (self.next() % (i as u64 + 1)) as usize);
        }
    }
}

/// Page bytes `key` with `payload` takes in a leaf of `shape`.
pub(crate) fn span(shape: Shape, key: &[u8], payload: &[u8]) -> usize {
    Leaf::new(shape).span_of(&Ent { key, vlen: 0, payload })
}

/// Every leaf but the tree's first and last holds at least [`FILL`]
/// bytes less two of the widest entry the tree has held; returns the
/// least and mean fill of those leaves.
pub(crate) fn check_fill(t: &Tree, widest: usize) -> (f64, f64) {
    let (mut least, mut sum, mut n) = (usize::MAX, 0, 0);
    let mut id = t.first;
    while id != NIL {
        let l = t.leaf(id);
        if l.prev != NIL && l.next != NIL {
            assert!(l.used() + 2 * widest >= FILL, "leaf {id} holds {} bytes", l.used());
            (least, sum, n) = (least.min(l.used()), sum + l.used(), n + 1);
        }
        id = l.next;
    }
    let cap = Leaf::capacity() as f64;
    (least.min(Leaf::capacity()) as f64 / cap, sum as f64 / cap / n.max(1) as f64)
}

/// Leaves the invariant allows for `bytes` of entries no wider than
/// `widest`: two ends plus the rest at the least fill.
fn most_leaves(bytes: usize, widest: usize) -> usize {
    2 + bytes / (FILL - 2 * widest)
}

/// Order keys shaped like an i64 index over `user:<id>` rows with
/// `age = id % 100`: the value's 8 bytes, then the id's digits packed.
fn entry(id: u32) -> Vec<u8> {
    let mut e = (u64::from(id % 100) ^ (1 << 63)).to_be_bytes().to_vec();
    crate::seg_codec::pack_digits(id.to_string().as_bytes(), &mut e);
    e
}

/// Ids in the orders a write stream can bring them.
fn orders(n: u32) -> Vec<(&'static str, Vec<u32>)> {
    let ids: Vec<u32> = (0..n).collect();
    let mut sorted = ids.clone();
    sorted.sort_by_key(|&i| entry(i));
    let reverse = sorted.iter().rev().copied().collect();
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut random = ids.clone();
    r.shuffle(&mut random);
    let n = n as usize;
    let zigzag = (0..n).map(|i| sorted[if i % 2 == 0 { i / 2 } else { n - 1 - i / 2 }]).collect();
    // four shards' ascending streams arriving interleaved at random
    let mut queues: Vec<Vec<u32>> =
        (0..4).map(|q| ids.iter().copied().filter(|i| i % 4 == q).rev().collect()).collect();
    let mut shards = Vec::new();
    while queues.iter().any(|q| !q.is_empty()) {
        let q = (r.next() % 4) as usize;
        shards.extend(queues[q].pop());
    }
    vec![
        ("sorted", sorted),
        ("reverse", reverse),
        ("random", random),
        ("streams", ids),
        ("zigzag", zigzag),
        ("shards", shards),
    ]
}

#[test]
fn every_leaf_but_the_ends_stays_two_thirds_full_whatever_the_order() {
    let shape = Shape { payloads: false, vlens: false };
    for (name, ids) in orders(20_000) {
        let mut t = Tree::new(shape);
        let mut want = BTreeSet::new();
        let (mut widest, mut bytes) = (0, 0);
        for &i in &ids {
            let e = entry(i);
            let b = span(shape, &e, &[]);
            (widest, bytes) = (widest.max(b), bytes + b);
            assert!(t.insert(&e, &[]));
            want.insert(e);
        }
        assert!(t.height >= 2, "{name}: leaves under several parents");
        check(&t);
        check_fill(&t, widest);
        assert!(
            t.live_leaves() <= most_leaves(bytes, widest),
            "{name}: {} leaves",
            t.live_leaves()
        );
        // delete a random third of what is left, batch after batch
        let mut r = Rng(0x2545_F491_4F6C_DD1D);
        let mut live: Vec<Vec<u8>> = want.iter().cloned().collect();
        r.shuffle(&mut live);
        while !live.is_empty() {
            let batch = live.len().div_ceil(3);
            for e in live.drain(..batch) {
                assert!(t.remove(&e));
                bytes -= span(shape, &e, &[]);
                want.remove(&e);
            }
            let got: Vec<Vec<u8>> = check(&t).into_iter().map(|(k, _)| k).collect();
            assert!(got.iter().eq(want.iter()), "{name}: entries after a delete batch");
            check_fill(&t, widest);
            assert!(t.live_leaves() <= most_leaves(bytes, widest), "{name}: after deletes");
        }
        assert_eq!(t.live_leaves(), 0);
    }
}

#[test]
fn payloads_rewritten_shorter_refill_their_leaves() {
    let shape = Shape { payloads: true, vlens: false };
    let mut t = Tree::new(shape);
    let (_, mut ids) = orders(20_000).swap_remove(2);
    for &i in &ids {
        t.insert(&entry(i), &[7; 40]);
    }
    let widest = span(shape, &entry(19_999), &[7; 40]);
    check_fill(&t, widest);
    Rng(3).shuffle(&mut ids);
    for (n, &i) in ids.iter().enumerate() {
        assert!(!t.insert(&entry(i), &[1]), "a rewrite, not a new key");
        if n % 997 == 0 {
            check_fill(&t, widest);
        }
    }
    assert_eq!(check(&t).len(), ids.len());
    check_fill(&t, widest);
}

#[test]
fn an_index_segment_keeps_the_bound_in_every_order() {
    for (name, ids) in orders(20_000) {
        let mut s = Segment::with_codec(Codec::new(Form::I64, b"user:", 0));
        for &i in &ids {
            s.apply(
                format!("user:{i}").as_bytes(),
                None,
                Some(IndexValue::I64(i64::from(i % 100))),
            );
        }
        // a 10-byte slot, the tag, and at most 3 packed digits past the value
        let widest = 10 + 1 + 3;
        let (least, mean) = check_fill(&s.tree, widest);
        let leaves = s.tree.live_leaves();
        assert!(leaves <= most_leaves(ids.len() * widest, widest), "{name}: {leaves} leaves");
        let per_row = s.stats().approx_bytes as f64 / ids.len() as f64;
        eprintln!(
            "{name:>8}: {leaves} leaves, fill least {least:.3} mean {mean:.3}, {per_row:.1} B/row"
        );
    }
}
