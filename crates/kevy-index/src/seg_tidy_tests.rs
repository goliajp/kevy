//! The background repack against a model, and what it converges to.

use std::collections::BTreeMap;

use super::tests::{Rng, check, key_for, ranks_agree};
use super::*;
use crate::seg_codec::{Codec, Form};
use crate::seg_leaf::Shape;
use crate::{IndexValue, Segment};

/// Step the hand until the tree rests; the steps it took.
fn run_to_rest(t: &mut Tree, tidy: &mut Tidy) -> usize {
    let cap = 4 * t.live_leaves() + 16;
    let mut steps = 0;
    while t.tidy(tidy, 1) {
        steps += 1;
        assert!(steps <= cap, "a lap over {} leaves never ends", t.live_leaves());
    }
    steps
}

/// A tree at rest: every leaf but the last is too full to take its
/// successor's first entry.
fn check_packed(t: &Tree) {
    let mut id = t.first;
    while id != NIL {
        let l = t.leaf(id);
        if l.next != NIL {
            let head = t.leaf(l.next).span_bytes(0, 1);
            assert!(l.used() + head > Leaf::capacity(), "leaf {id} holds {} with room", l.used());
        }
        id = l.next;
    }
}

/// Every entry once each way, in order.
fn walks(t: &Tree) -> (usize, usize) {
    let (mut fwd, mut back) = (0, 0);
    let mut p = t.first_pos();
    while let Some(q) = p {
        (fwd, p) = (fwd + 1, t.next_pos(q));
    }
    let mut p = t.last_pos();
    while let Some(q) = p {
        (back, p) = (back + 1, t.prev_pos(q));
    }
    (fwd, back)
}

fn model_run(seed: u64, payloads: bool, ops: usize) {
    let mut r = Rng(seed);
    let mut t = Tree::new(Shape { payloads, vlens: false });
    let mut tidy = Tidy::default();
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for step in 0..ops {
        let shape = r.below(10).min(2).max(r.below(2));
        let key = key_for(&mut r, shape);
        let payload: Vec<u8> =
            if payloads { vec![step as u8; r.below(40) as usize] } else { Vec::new() };
        match r.below(12) {
            0..=5 => assert_eq!(t.insert(&key, &payload), m.insert(key, payload).is_none()),
            6..=8 => assert_eq!(t.remove(&key), m.remove(&key).is_some()),
            9 => {
                t.cut_below(&Probe::new(&key), |_| {});
                m = m.split_off(&key);
            }
            _ => {
                t.tidy(&mut tidy, r.below(6) as usize);
            }
        }
        if step % 7 == 0 || step == ops - 1 {
            let want: Vec<(Vec<u8>, Vec<u8>)> =
                m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(check(&t), want, "step {step}");
            ranks_agree(&t, &m, &mut r);
        }
    }
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
    assert_eq!(check(&t).len(), m.len());
    assert_eq!(walks(&t), (m.len(), m.len()));
    ranks_agree(&t, &m, &mut r);
}

#[test]
fn packing_between_writes_keeps_order_counts_and_ranks() {
    for seed in 1..=12u64 {
        model_run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), seed % 2 == 0, 6000);
    }
}

/// Order keys shaped like an i64 index over `user:<id>` with `age = id %
/// 100`: the value's 8 bytes, then the id's digits packed.
fn entry(id: u32) -> Vec<u8> {
    let mut e = (u64::from(id % 100) ^ (1 << 63)).to_be_bytes().to_vec();
    crate::seg_codec::pack_digits(id.to_string().as_bytes(), &mut e);
    e
}

#[test]
fn a_rested_tree_packs_every_leaf_but_the_last_and_wakes_on_drift() {
    let mut t = Tree::new(Shape { payloads: false, vlens: false });
    let mut tidy = Tidy::default();
    let mut r = Rng(5);
    let mut ids: Vec<u32> = (0..40_000).collect();
    for i in (1..ids.len()).rev() {
        ids.swap(i, r.below(i as u64 + 1) as usize);
    }
    for &i in &ids[..20_000] {
        t.insert(&entry(i), &[]);
    }
    let before = t.live_leaves();
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
    assert!(t.live_leaves() * 4 < before * 3, "{} of {before} leaves", t.live_leaves());
    assert!(!t.tidy(&mut tidy, 1_000), "a rested tree does nothing");
    // random writes split packed leaves in half: past an eighth more
    // leaves a row, the hand walks again
    let mut woke = false;
    for &i in &ids[20_000..] {
        t.insert(&entry(i), &[]);
        woke |= t.tidy(&mut tidy, 0);
    }
    assert!(woke, "writes that loosen the tree wake the hand");
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
    assert_eq!(check(&t).len(), 40_000);
}

#[test]
fn writes_alone_leave_no_fill_floor() {
    // an entry landing just past a full leaf whose right neighbour is
    // full too opens a leaf of its own, however the rest is packed
    let mut t = Tree::new(Shape { payloads: false, vlens: false });
    for i in 0..20_000u32 {
        t.insert(&(2 * i).to_be_bytes(), &[]);
    }
    let second = t.leaf(t.first).next;
    let mut last = Vec::new();
    t.leaf(second).key_into(t.leaf(second).len() - 1, &t.ov, &mut last);
    let past = u32::from_be_bytes(last[..4].try_into().unwrap()) + 1;
    t.insert(&past.to_be_bytes(), &[]);
    let opened = t.leaf(t.leaf(second).next);
    assert_eq!(opened.len(), 1, "an interior leaf with one entry");
    let mut tidy = Tidy::default();
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
}

#[test]
fn an_index_segment_packs_whatever_order_its_rows_came_in() {
    let n = 20_000u32;
    let ids: Vec<u32> = (0..n).collect();
    let mut sorted = ids.clone();
    sorted.sort_by_key(|&i| entry(i));
    let mut random = ids.clone();
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for i in (1..random.len()).rev() {
        random.swap(i, r.below(i as u64 + 1) as usize);
    }
    for (name, order) in [("sorted", sorted), ("random", random), ("streams", ids)] {
        let mut s = Segment::with_codec(Codec::new(Form::I64, b"user:", 0));
        for &i in &order {
            let v = IndexValue::I64(i64::from(i % 100));
            s.apply(format!("user:{i}").as_bytes(), None, Some(v));
        }
        let written = s.stats().approx_bytes as f64 / f64::from(n);
        let steps = std::iter::from_fn(|| s.tidy(1).then_some(())).count();
        check_packed(&s.tree);
        let packed = s.stats().approx_bytes as f64 / f64::from(n);
        eprintln!("{name:>8}: {written:.1} B/row written, {packed:.1} at rest after {steps} steps");
        // a 10-byte slot, the tag and at most 3 packed digits past the value
        let widest = 14;
        let most = 1 + (n as usize * widest) / (Leaf::capacity() - widest);
        assert!(s.tree.live_leaves() <= most, "{name}: {} leaves", s.tree.live_leaves());
    }
}
