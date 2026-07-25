//! White-box invariant checks: after arbitrary operation sequences, every
//! node must hold the B-tree shape rules AND the order-statistic `total`
//! bookkeeping. The black-box oracle test (`tests/oracle.rs`) checks
//! answers; this one checks the structure those answers rest on.

use crate::RankTree;
use crate::node::{MAX_KEYS, MIN_KEYS, Node};

/// Walk the subtree, asserting every structural invariant. Returns
/// `(key count, depth)` so parents can check totals and leaf uniformity.
fn check_node<K: Ord + core::fmt::Debug>(node: &Node<K>, is_root: bool) -> (usize, usize) {
    assert!(node.keys.len() <= MAX_KEYS, "overfull node: {}", node.keys.len());
    if !is_root {
        assert!(node.keys.len() >= MIN_KEYS, "underfull node: {}", node.keys.len());
    }
    assert!(node.keys.is_sorted(), "unsorted keys");
    if node.is_leaf() {
        assert_eq!(node.total, node.keys.len(), "leaf total drifted");
        return (node.keys.len(), 0);
    }
    assert_eq!(node.children.len(), node.keys.len() + 1, "child count");
    let mut count = node.keys.len();
    let mut depth = None;
    for (i, c) in node.children.iter().enumerate() {
        // Separator ordering: child i < keys[i] < child i+1.
        if i > 0 {
            assert!(c.keys.first().unwrap() > &node.keys[i - 1], "separator order (left)");
        }
        if i < node.keys.len() {
            assert!(c.keys.last().unwrap() < &node.keys[i], "separator order (right)");
        }
        let (n, d) = check_node(c, false);
        count += n;
        assert_eq!(*depth.get_or_insert(d), d, "leaves at unequal depths");
    }
    assert_eq!(node.total, count, "internal total drifted");
    (count, depth.unwrap() + 1)
}

fn check<K: Ord + core::fmt::Debug>(t: &RankTree<K>) {
    check_node(&t.root, true);
}

/// Deterministic splitmix64 so the sequences are replayable.
struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[test]
fn ascending_and_descending_bulk_hold_invariants() {
    let mut t = RankTree::new();
    for i in 0..2_000u32 {
        assert!(t.insert(i));
        if i % 97 == 0 {
            check(&t);
        }
    }
    check(&t);
    for i in (0..2_000u32).rev() {
        assert!(t.remove(&i));
        if i % 97 == 0 {
            check(&t);
        }
    }
    assert!(t.is_empty());
    check(&t);
}

#[test]
fn random_churn_holds_invariants() {
    let mut rng = SplitMix(0xC0FF_EE00);
    let mut t = RankTree::new();
    let mut live = alloc::vec::Vec::new();
    for step in 0..20_000 {
        let x = (rng.next() % 4_096) as u32;
        if rng.next().is_multiple_of(3) {
            let removed = t.remove(&x);
            assert_eq!(removed, live.contains(&x), "remove({x}) presence");
            live.retain(|&v| v != x);
        } else {
            let added = t.insert(x);
            assert_eq!(added, !live.contains(&x), "insert({x}) novelty");
            if added {
                live.push(x);
            }
        }
        assert_eq!(t.len(), live.len());
        if step % 501 == 0 {
            check(&t);
        }
    }
    check(&t);
}

#[test]
fn every_small_permutation_of_inserts_and_removes() {
    // Exhaustive-ish: for every n up to two split levels, insert 0..n in a
    // rotated order, then remove in a differently rotated order, checking
    // invariants at every single step. Exercises each split/borrow/merge
    // arity at least once (n crosses 15, 16, 31, 32 node boundaries).
    for n in [1usize, 2, 15, 16, 17, 31, 32, 33, 64, 255, 256, 257] {
        for rot in [0usize, 1, n / 2, n.saturating_sub(1)] {
            let mut t = RankTree::new();
            for i in 0..n {
                assert!(t.insert((i + rot) % n));
                check(&t);
            }
            for i in 0..n {
                assert!(t.remove(&((i + n / 3) % n)));
                check(&t);
            }
            assert!(t.is_empty());
        }
    }
}
