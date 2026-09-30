use std::collections::BTreeMap;

use super::*;
use crate::seg_leaf::{Shape, head_of};

/// Every structural invariant, checked from scratch; returns the entries
/// in order.
pub(crate) fn check(t: &Tree) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut leaves = Vec::new();
    if t.root == NIL {
        assert_eq!(
            (t.len, t.first, t.live_leaves(), t.live_inners()),
            (0, NIL, 0, 0),
            "an empty tree holds nothing"
        );
        return out;
    }
    let n = walk(t, t.root, t.height, None, None, &mut out, &mut leaves);
    assert_eq!(n, t.len, "len");
    assert_eq!(out.len(), t.len);
    for w in out.windows(2) {
        assert!(w[0].0 < w[1].0, "strictly increasing: {:?} then {:?}", w[0].0, w[1].0);
    }
    // the leaf chain is the in-order leaf list, both ways
    assert_eq!(t.first, leaves[0], "first leaf");
    for w in leaves.windows(2) {
        assert_eq!(t.leaf(w[0]).next, w[1]);
        assert_eq!(t.leaf(w[1]).prev, w[0]);
    }
    assert_eq!(t.leaf(*leaves.last().unwrap()).next, NIL);
    assert_eq!(t.leaf(leaves[0]).prev, NIL);
    if t.height > 0 {
        assert!(leaves.iter().all(|&l| !t.leaf(l).is_empty()), "no empty leaf under an inner node");
    }
    let seps: usize =
        live_inners(t).map(|i| t.inners[i].seps.iter().map(|s| s.len()).sum::<usize>()).sum();
    assert_eq!(seps, t.sep_bytes, "separator bytes");
    let slabs: usize = leaves
        .iter()
        .flat_map(|&l| (0..t.leaf(l).len()).map(move |i| (l, i)))
        .filter_map(|(l, i)| t.leaf(l).slab_of(i))
        .map(|id| t.ov.get(id).len())
        .sum();
    assert_eq!(slabs, t.ov.bytes, "overflow bytes are exactly the live slabs");
    assert_eq!(t.live_leaves(), leaves.len(), "no leaked leaf");
    out
}

fn live_inners(t: &Tree) -> impl Iterator<Item = usize> + '_ {
    (0..t.inners.len()).filter(|&i| !t.free_inners.contains(&(i as u32)))
}

fn walk(
    t: &Tree,
    node: u32,
    h: usize,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    out: &mut Vec<(Vec<u8>, Vec<u8>)>,
    leaves: &mut Vec<u32>,
) -> usize {
    if h == 0 {
        let l = t.leaf(node);
        leaves.push(node);
        let mut key = Vec::new();
        for i in 0..l.len() {
            let payload = t.entry(Pos { leaf: node, slot: i }, &mut key).to_vec();
            assert_eq!(head_of(&key), l.head(i), "head matches key");
            if let Some(lo) = lo {
                assert!(key.as_slice() >= lo, "entry at or above its separator");
            }
            if let Some(hi) = hi {
                assert!(key.as_slice() < hi, "entry below the next separator");
            }
            out.push((key.clone(), payload));
        }
        return l.len();
    }
    let inner = &t.inners[node as usize];
    assert_eq!(inner.kids.len(), inner.seps.len() + 1, "kids and separators");
    assert!(inner.kids.len() <= FANOUT, "fanout");
    assert!(!inner.kids.is_empty());
    let mut total = 0;
    for (j, &kid) in inner.kids.iter().enumerate() {
        if j < inner.seps.len() {
            assert_eq!(inner.heads[j], crate::seg_leaf::head16_of(&inner.seps[j]));
        }
        let klo = if j == 0 { lo } else { Some(&*inner.seps[j - 1]) };
        let khi = if j < inner.seps.len() { Some(&*inner.seps[j]) } else { hi };
        let n = walk(t, kid, h - 1, klo, khi, out, leaves);
        assert_eq!(inner.counts[j] as usize, n, "count of child {j}");
        total += n;
    }
    total
}

const CHECK_EVERY: usize = 7;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn key_for(r: &mut Rng, shape: u64) -> Vec<u8> {
    match shape {
        // 12-byte keys over a small space, so inserts hit existing keys
        0 => [r.below(400).to_be_bytes().as_slice(), &[0, 0, 0, 1]].concat(),
        // mixed lengths, some sharing long prefixes
        1 => {
            let n = r.below(20) as usize;
            let mut k = vec![b'k'; n];
            if n > 0 {
                k[n - 1] = r.below(4) as u8;
            }
            k
        }
        // a few huge keys that go out of line
        _ => vec![r.below(3) as u8; 300 + r.below(3) as usize * 200],
    }
}

fn run_model(seed: u64, payloads: bool, ops: usize) {
    let mut r = Rng(seed);
    let form = Shape { payloads, vlens: false };
    let mut t = Tree::new(form);
    let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut widest = 0;
    for step in 0..ops {
        let shape = r.below(10).min(2).max(r.below(2));
        let key = key_for(&mut r, shape);
        let payload: Vec<u8> =
            if payloads { vec![step as u8; r.below(40) as usize] } else { Vec::new() };
        match r.below(10) {
            0..=5 => {
                widest = widest.max(super::fill_tests::span(form, &key, &payload));
                assert_eq!(t.insert(&key, &payload), m.insert(key, payload).is_none());
            }
            6..=8 => assert_eq!(t.remove(&key), m.remove(&key).is_some()),
            _ => {
                let mut cut = Vec::new();
                t.cut_below(&Probe::new(&key), |e| cut.push((e.key.to_vec(), e.payload.to_vec())));
                let keep = m.split_off(&key);
                let gone: Vec<(Vec<u8>, Vec<u8>)> =
                    std::mem::replace(&mut m, keep).into_iter().collect();
                assert_eq!(cut, gone, "cut below {key:?}");
            }
        }
        if step % CHECK_EVERY == 0 || step == ops - 1 {
            let got = check(&t);
            let want: Vec<(Vec<u8>, Vec<u8>)> =
                m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(got, want, "step {step}");
            super::fill_tests::check_fill(&t, widest, super::balance::HALF);
            ranks_agree(&t, &m, &mut r);
        }
    }
    t.repack();
    let got = check(&t);
    assert_eq!(got.len(), m.len());
}

fn ranks_agree(t: &Tree, m: &BTreeMap<Vec<u8>, Vec<u8>>, r: &mut Rng) {
    for _ in 0..20 {
        let shape = r.below(3);
        let k = key_for(r, shape);
        let below = m.range(..k.clone()).count();
        assert_eq!(t.rank(&Probe::new(&k)), below, "rank {k:?}");
        let upto = m.keys().filter(|x| x.as_slice() < k.as_slice() || x.starts_with(&k)).count();
        assert_eq!(t.rank(&Probe::past(&k)), upto, "rank past {k:?}");
        let lb = t.lower_bound(&Probe::new(&k)).map(|p| {
            let mut key = Vec::new();
            t.entry(p, &mut key);
            key
        });
        assert_eq!(lb, m.range(k.clone()..).next().map(|(x, _)| x.clone()), "lower bound {k:?}");
    }
}

#[test]
fn random_operations_match_a_map() {
    for seed in 1..=12u64 {
        run_model(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), seed % 2 == 0, 6000);
    }
}

#[test]
fn ascending_and_descending_runs_pack_leaves_full() {
    for desc in [false, true] {
        let mut t = Tree::new(Shape { payloads: false, vlens: false });
        for i in 0..20_000u64 {
            let v = if desc { u64::MAX - i } else { i };
            t.insert(&[v.to_be_bytes().as_slice(), &[1, 2, 3, 4]].concat(), &[]);
        }
        check(&t);
        let fill = t.len as f64 * 15.0 / (t.live_leaves() * Leaf::capacity()) as f64;
        assert!(fill > 0.95, "desc={desc}: fill {fill:.3}");
    }
}

#[test]
fn random_inserts_fill_leaves_past_two_thirds_and_repack_fills_them() {
    let mut r = Rng(7);
    let mut t = Tree::new(Shape { payloads: false, vlens: false });
    for _ in 0..50_000 {
        t.insert(&[r.next().to_be_bytes().as_slice(), &[1, 2, 3, 4]].concat(), &[]);
    }
    let fill = |t: &Tree| t.len as f64 * 15.0 / (t.live_leaves() * Leaf::capacity()) as f64;
    let before = fill(&t);
    assert!(before > 0.8, "random fill {before:.3}");
    t.repack();
    check(&t);
    assert!(fill(&t) > 0.95, "repacked fill {:.3}", fill(&t));
}

#[test]
fn stepping_both_ways_visits_every_entry() {
    let mut t = Tree::new(Shape { payloads: false, vlens: false });
    for i in 0..5000u32 {
        t.insert(&(i * 7 % 5000).to_be_bytes(), &[]);
    }
    let mut n = 0;
    let mut pos = t.first_pos();
    while let Some(p) = pos {
        n += 1;
        pos = t.next_pos(p);
    }
    assert_eq!(n, 5000);
    let mut n = 0;
    let mut pos = t.last_pos();
    while let Some(p) = pos {
        n += 1;
        pos = t.prev_pos(p);
    }
    assert_eq!(n, 5000);
    let p = t.before(&Probe::new(&100u32.to_be_bytes())).expect("99 is there");
    let mut key = Vec::new();
    t.entry(p, &mut key);
    assert_eq!(key, 99u32.to_be_bytes());
}

#[test]
fn thinning_a_deep_tree_merges_leaves_and_keeps_it_whole() {
    let mut r = Rng(11);
    let mut t = Tree::new(Shape { payloads: true, vlens: false });
    let mut m = BTreeMap::new();
    for i in 0..40_000u32 {
        let k = (i.wrapping_mul(2_654_435_761)).to_be_bytes().to_vec();
        t.insert(&k, &[i as u8; 3]);
        m.insert(k, vec![i as u8; 3]);
    }
    let leaves = t.live_leaves();
    let keys: Vec<Vec<u8>> = m.keys().cloned().collect();
    for (n, k) in keys.iter().enumerate() {
        if r.below(10) < 9 {
            assert!(t.remove(k));
            m.remove(k);
        }
        if n % 4999 == 0 {
            let got = check(&t);
            assert_eq!(got, m.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
        }
    }
    check(&t);
    assert!(t.live_leaves() * 3 < leaves, "thinned leaves merged: {} of {leaves}", t.live_leaves());
}

#[test]
fn removing_everything_leaves_no_leaf() {
    let mut t = Tree::new(Shape { payloads: true, vlens: false });
    for i in 0..3000u32 {
        t.insert(&i.to_be_bytes(), b"x");
    }
    for i in 0..3000u32 {
        assert!(t.remove(&i.to_be_bytes()));
    }
    check(&t);
    assert_eq!((t.len, t.height, t.live_leaves()), (0, 0, 0), "the last leaf went too");
    assert!(t.first_pos().is_none() && t.last_pos().is_none());
}
