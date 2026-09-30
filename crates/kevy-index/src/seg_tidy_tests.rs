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

/// A key over a space wide enough for a tree of several levels, now and
/// then one of the long or out-of-line shapes.
fn model_key(r: &mut Rng) -> Vec<u8> {
    match r.below(20) {
        0 => key_for(r, 1),
        1 => key_for(r, 2),
        _ => [(r.below(200_000) as u32).to_be_bytes().as_slice(), b"row:tail"].concat(),
    }
}

fn model_run(seed: u64, payloads: bool, ops: usize) {
    let mut r = Rng(seed);
    let mut t = Tree::new(Shape { payloads, vlens: false });
    let mut tidy = Tidy::default();
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let (mut most, mut packed) = (0, false);
    for step in 0..ops {
        let key = model_key(&mut r);
        let payload: Vec<u8> =
            if payloads { vec![step as u8; r.below(40) as usize] } else { Vec::new() };
        match r.below(10_000) {
            0..=5_499 => assert_eq!(t.insert(&key, &payload), m.insert(key, payload).is_none()),
            5_500..=8_499 => assert_eq!(t.remove(&key), m.remove(&key).is_some()),
            8_500 if step > ops / 2 => {
                t.cut_below(&Probe::new(&key), |_| {});
                m = m.split_off(&key);
            }
            _ => {
                let before = t.live_leaves();
                t.tidy(&mut tidy, r.below(6) as usize);
                packed |= t.live_leaves() < before;
            }
        }
        most = most.max(t.height);
        if step % 97 == 0 || step == ops - 1 {
            let want: Vec<(Vec<u8>, Vec<u8>)> =
                m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(check(&t), want, "step {step}");
            ranks_agree(&t, &m, &mut r);
        }
    }
    assert!(most >= 2 && packed, "the run reached {most} levels and emptied a leaf: {packed}");
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
    let want: Vec<(Vec<u8>, Vec<u8>)> = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(check(&t), want);
    assert_eq!(walks(&t), (m.len(), m.len()));
    ranks_agree(&t, &m, &mut r);
}

#[test]
fn packing_between_writes_keeps_order_counts_and_ranks() {
    for seed in 1..=8u64 {
        model_run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), seed % 2 == 0, 20_000);
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
    assert_eq!(check(&t).len(), 20_000);
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
    // deletes hollow leaves without adding any: an eighth fewer entries
    // wakes it too
    let mut woke = false;
    for &i in &ids[..6_000] {
        t.remove(&entry(i));
        woke |= t.tidy(&mut tidy, 0);
    }
    assert!(woke, "deletes that hollow the tree wake the hand");
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
    assert_eq!(check(&t).len(), 34_000);
}

#[test]
fn a_tree_that_rested_small_still_wakes_as_it_grows() {
    let mut t = Tree::new(Shape { payloads: false, vlens: false });
    let mut tidy = Tidy::default();
    t.insert(&entry(1), &[]);
    assert!(!t.tidy(&mut tidy, 4), "one leaf rests at once");
    let mut r = Rng(9);
    for _ in 0..20_000 {
        t.insert(&entry(r.below(1_000_000) as u32), &[]);
    }
    assert!(t.tidy(&mut tidy, 0), "a grown tree wakes the hand");
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
}

#[test]
fn deletes_alone_leave_no_fill_floor() {
    // a leaf thinned to one entry between two full neighbours stays: a
    // delete merges a leaf under a quarter full only into a neighbour
    // the two fit in three-quarters of
    let mut t = Tree::new(Shape { payloads: false, vlens: false });
    for i in 0..20_000u32 {
        t.insert(&i.to_be_bytes(), &[]);
    }
    let mid = t.leaf(t.leaf(t.first).next).next;
    let n = t.leaf(mid).len();
    let mut keys = Vec::new();
    for i in 1..n {
        let mut k = Vec::new();
        t.leaf(mid).key_into(i, &t.ov, &mut k);
        keys.push(k);
    }
    for k in &keys {
        assert!(t.remove(k));
    }
    assert_eq!(t.leaf(mid).len(), 1, "an interior leaf with one entry");
    let mut tidy = Tidy::default();
    run_to_rest(&mut t, &mut tidy);
    check_packed(&t);
    assert_eq!(check(&t).len(), 20_000 - keys.len());
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
    // miri interprets every step; a thousand rows still span many leaves
    let n = if cfg!(miri) { 500u32 } else { 20_000 };
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
        let steps = run_to_rest(&mut s.tree, &mut s.tidy);
        check_packed(&s.tree);
        let packed = s.stats().approx_bytes as f64 / f64::from(n);
        eprintln!("{name:>8}: {written:.1} B/row written, {packed:.1} at rest after {steps} steps");
        // a 10-byte slot, the tag and at most 3 packed digits past the value
        let widest = 14;
        let most = 1 + (n as usize * widest) / (Leaf::capacity() - widest);
        assert!(s.tree.live_leaves() <= most, "{name}: {} leaves", s.tree.live_leaves());
    }
}
