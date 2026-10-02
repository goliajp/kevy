//! The index against a sorted vector, with its structure checked after
//! every batch: counts, separators, fill, one leaf depth, whole
//! permutations.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Ordering;

use super::inner::{INNER_CAP, INNER_MIN};
use super::leaf::{LEAF_CAP, LEAF_MIN, Leaf};
use super::{Node, ZIndex, key_score, score_key};
use crate::value::SmallBytes;

type Model = Vec<(f64, Vec<u8>)>;

fn cmp(a: &(f64, Vec<u8>), b: &(f64, Vec<u8>)) -> Ordering {
    a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1))
}

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

type Key = Option<(u64, Vec<u8>)>;

/// `(height, entries, smallest key, largest key)` of a subtree, its
/// structure checked on the way.
fn check(node: &Node, root: bool) -> (usize, usize, Key, Key) {
    match node {
        Node::Leaf(l) => {
            assert!(l.perm_is_whole(), "permutation lists every slot once");
            assert!(root || l.len() >= LEAF_MIN, "leaf of {} below half", l.len());
            assert!(l.len() <= LEAF_CAP);
            let key = |i: usize| (l.score(i), l.member(i).to_vec());
            for i in 1..l.len() {
                assert!(key(i - 1) < key(i), "leaf order");
            }
            (0, l.len(), (l.len() > 0).then(|| key(0)), (l.len() > 0).then(|| key(l.len() - 1)))
        }
        Node::Inner(n) => {
            assert!(n.len() >= if root { 2 } else { INNER_MIN }, "inner of {} below half", n.len());
            assert!(n.len() <= INNER_CAP);
            let mut height = None;
            let (mut lo, mut hi) = (None, None);
            for i in 0..n.len() {
                let (h, count, kid_lo, kid_hi) = check(n.kid(i), false);
                assert_eq!(count, n.count(i), "child {i} count");
                assert_eq!(*height.get_or_insert(h), h, "leaves at one depth");
                if i > 0 {
                    let sep = n.sep(i - 1);
                    let sep = (sep.0, sep.1.to_vec());
                    assert!(hi.as_ref().is_none_or(|h| *h < sep), "separator above the left child");
                    assert!(
                        kid_lo.as_ref().is_none_or(|l| sep <= *l),
                        "separator at or below the right child"
                    );
                }
                lo = lo.or(kid_lo);
                hi = kid_hi.or(hi);
            }
            (height.unwrap_or(0) + 1, n.total(), lo, hi)
        }
    }
}

fn check_tree(z: &ZIndex, model: &Model) {
    match z.root.as_ref() {
        Some(root) => assert_eq!(check(root, true).1, model.len()),
        None => assert!(model.is_empty()),
    }
    assert_eq!(z.len(), model.len());
    let got: Vec<(f64, Vec<u8>)> = z.iter().map(|(m, s)| (s, m.to_vec())).collect();
    assert_eq!(got.len(), model.len());
    for (g, w) in got.iter().zip(model) {
        assert!(g.0.to_bits() == w.0.to_bits() && g.1 == w.1, "order: {g:?} vs {w:?}");
    }
}

fn member(i: u64) -> Vec<u8> {
    // short ones inline, every eighth past the inline length
    if i.is_multiple_of(8) {
        alloc::format!("member-with-a-long-name-{i:08}")
    } else {
        alloc::format!("m{i}")
    }
    .into_bytes()
}

#[test]
fn score_keys_order_as_total_cmp_and_round_trip() {
    let xs = [
        f64::NEG_INFINITY,
        -1e300,
        -1.5,
        -f64::MIN_POSITIVE,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        1.0,
        2.5,
        1e300,
        f64::INFINITY,
        f64::NAN,
        -f64::NAN,
        f64::from_bits(1),
        -f64::from_bits(1),
    ];
    for a in xs {
        assert_eq!(key_score(score_key(a)).to_bits(), a.to_bits());
        for b in xs {
            assert_eq!(score_key(a).cmp(&score_key(b)), a.total_cmp(&b), "{a} vs {b}");
        }
    }
}

#[test]
fn a_leaf_permutation_tracks_a_vector() {
    let mut rng = Rng(0x51_7cc1_b727_220a);
    let mut l = Leaf::new();
    let mut model: Vec<u64> = Vec::new();
    for _ in 0..20_000 {
        if model.len() < LEAF_CAP && (model.is_empty() || rng.below(2) == 0) {
            let at = rng.below(model.len() as u64 + 1) as usize;
            let v = rng.next();
            l.insert_at(at, v, SmallBytes::from_slice(&v.to_le_bytes()));
            model.insert(at, v);
        } else {
            let at = rng.below(model.len() as u64) as usize;
            let (v, m) = l.remove_at(at);
            assert_eq!((v, m.as_slice()), (model[at], &model.remove(at).to_le_bytes()[..]));
        }
        assert!(l.perm_is_whole());
        assert_eq!(l.len(), model.len());
        assert!((0..l.len()).all(|i| l.score(i) == model[i]));
    }
}

/// Random inserts, removals (present and absent) and duplicate inserts;
/// structure, ranks, iteration from a rank and score partitions checked
/// as they go.
fn random_run(seed: u64, ops: usize, scores: u64) {
    let mut rng = Rng(seed);
    let mut z = ZIndex::default();
    let mut model: Model = Vec::new();
    for op in 0..ops {
        let key = ((rng.below(scores) as f64) - (scores / 2) as f64, member(rng.below(ops as u64)));
        let at = model.binary_search_by(|e| cmp(e, &key));
        if rng.below(3) == 0 {
            assert_eq!(z.remove(key.0, &key.1), at.is_ok());
            if let Ok(i) = at {
                model.remove(i);
            }
        } else {
            assert_eq!(z.insert(key.0, SmallBytes::from_slice(&key.1)), at.is_err());
            if let Err(i) = at {
                model.insert(i, key);
            }
        }
        if op % 500 == 499 || op == ops - 1 {
            check_tree(&z, &model);
            spot_reads(&z, &model, &mut rng);
        }
    }
    // drain in random order, so every child position runs short
    while !model.is_empty() {
        let k = model.remove(rng.below(model.len() as u64) as usize);
        assert!(z.remove(k.0, &k.1));
        if model.len().is_multiple_of(300) {
            check_tree(&z, &model);
        }
    }
    assert!(z.root.is_none() && z.len() == 0);
}

fn spot_reads(z: &ZIndex, model: &Model, rng: &mut Rng) {
    for _ in 0..20 {
        let Some(r) = (!model.is_empty()).then(|| rng.below(model.len() as u64) as usize) else {
            break;
        };
        assert_eq!(z.rank_of(model[r].0, &model[r].1), Some(r));
        let tail: Vec<Vec<u8>> = z.iter_from(r).take(40).map(|(m, _)| m.to_vec()).collect();
        let want: Vec<Vec<u8>> = model[r..].iter().take(40).map(|e| e.1.clone()).collect();
        assert_eq!(tail, want, "iteration from rank {r}");
        let t = model[r].0;
        assert_eq!(z.partition(|s| s < t), model.partition_point(|e| e.0 < t));
        assert_eq!(z.partition(|s| s <= t), model.partition_point(|e| e.0 <= t));
    }
    for _ in 0..5 {
        let through = rng.below(model.len() as u64 + 2) as usize;
        let back: Vec<Vec<u8>> =
            z.iter_rev_through(through).take(40).map(|(m, _)| m.to_vec()).collect();
        let want: Vec<Vec<u8>> =
            model[..through.min(model.len())].iter().rev().take(40).map(|e| e.1.clone()).collect();
        assert_eq!(back, want, "backwards through {through}");
        let Some(e) = model.get(rng.below(model.len() as u64 + 1) as usize) else { continue };
        let k = (e.0, e.1.clone());
        let below = |s: f64, m: &[u8]| s.total_cmp(&k.0).then_with(|| m.cmp(&k.1)).is_lt();
        assert_eq!(z.partition_keys(below), model.partition_point(|x| cmp(x, &k).is_lt()));
    }
    assert_eq!(z.rank_of(0.25, b"absent"), None);
    assert_eq!(z.iter_from(model.len()).next(), None);
}

#[test]
fn random_operations_match_a_sorted_vector_with_ties() {
    random_run(0x9e37_79b9_7f4a_7c15, if cfg!(miri) { 1500 } else { 60_000 }, 40);
}

#[test]
fn random_operations_match_a_sorted_vector_with_spread_scores() {
    random_run(0x2545_f491_4f6c_dd1d, if cfg!(miri) { 1500 } else { 60_000 }, 1 << 40);
}

#[test]
fn one_score_for_everything_orders_by_member() {
    random_run(0xdead_beef_cafe_f00d, if cfg!(miri) { 1000 } else { 20_000 }, 1);
}

#[test]
fn building_from_sorted_entries_gives_a_whole_tree() {
    let sizes: &[usize] = if cfg!(miri) {
        &[0, 1, 13, 14, 16, 300]
    } else {
        &[0, 1, 7, 12, 13, 14, 15, 16, 25, 31, 299, 300, 301, 7_000, 40_000]
    };
    for &n in sizes {
        let model: Model = (0..n as u64).map(|i| ((i / 3) as f64, member(i))).collect::<Vec<_>>();
        let mut model = model;
        model.sort_by(cmp);
        let z = ZIndex::from_sorted(
            model.len(),
            model.iter().map(|(s, m)| (*s, SmallBytes::from_slice(m))),
        );
        check_tree(&z, &model);
        let mut rng = Rng(n as u64 + 1);
        spot_reads(&z, &model, &mut rng);
    }
}

/// Every node of a tree, by address.
fn nodes(node: &Node, out: &mut Vec<*const u8>) {
    match node {
        Node::Leaf(l) => out.push(Arc::as_ptr(l).cast()),
        Node::Inner(n) => {
            out.push(Arc::as_ptr(n).cast());
            (0..n.len()).for_each(|i| nodes(n.kid(i), out));
        }
    }
}

#[test]
fn a_write_under_a_snapshot_copies_its_path_only() {
    let n = if cfg!(miri) { 400 } else { 50_000 };
    let mut model: Model = (0..n).map(|i| (i as f64, member(i))).collect();
    model.sort_by(cmp);
    let mut z = ZIndex::from_sorted(
        model.len(),
        model.iter().map(|(s, m)| (*s, SmallBytes::from_slice(m))),
    );
    let snap = z.clone();
    let height = check(snap.root.as_ref().expect("built"), true).0;
    assert!(z.insert(0.5, SmallBytes::from_slice(b"new")));
    assert!(z.remove(model[7].0, &model[7].1));
    check_tree(&snap, &model);
    let (mut old, mut new) = (Vec::new(), Vec::new());
    nodes(snap.root.as_ref().expect("built"), &mut old);
    nodes(z.root.as_ref().expect("built"), &mut new);
    let copied = new.iter().filter(|p| !old.contains(p)).count();
    assert!(copied <= 2 * (height + 1), "{copied} nodes copied for two writes, height {height}");
    assert!(!snap.root_unique() || !z.root_unique() || copied > 0);
    drop(snap);
    assert!(z.root_unique());
}
