//! The oracle test: thousands of random insert/remove/rank/select/range
//! operations replayed against a sorted-`Vec` reference. Any divergence in
//! any answer fails with the operation index, so a failure is replayable
//! (the RNG is a seeded splitmix64 — no external crates).

use std::ops::Bound;

use kevy_ranktree::RankTree;

/// Deterministic splitmix64.
struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The reference: a sorted Vec with O(N) everything but obviously correct.
#[derive(Default)]
struct Oracle(Vec<u64>);
impl Oracle {
    fn insert(&mut self, k: u64) -> bool {
        match self.0.binary_search(&k) {
            Ok(_) => false,
            Err(i) => {
                self.0.insert(i, k);
                true
            }
        }
    }
    fn remove(&mut self, k: &u64) -> bool {
        match self.0.binary_search(k) {
            Ok(i) => {
                self.0.remove(i);
                true
            }
            Err(_) => false,
        }
    }
    fn rank_of(&self, k: &u64) -> Option<usize> {
        self.0.binary_search(k).ok()
    }
}

/// Compare every read-side answer for the current population.
fn compare_reads(step: usize, t: &RankTree<u64>, o: &Oracle, rng: &mut SplitMix) {
    assert_eq!(t.len(), o.0.len(), "step {step}: len");
    // Full forward and reverse iteration.
    let fwd: Vec<u64> = t.iter().copied().collect();
    assert_eq!(fwd, o.0, "step {step}: iter order");
    let rev: Vec<u64> = t.iter_rev().copied().collect();
    let mut want = o.0.clone();
    want.reverse();
    assert_eq!(rev, want, "step {step}: iter_rev order");
    // Probe ranks and selects at random points plus the edges.
    for _ in 0..8 {
        let probe = rng.below(1_024);
        assert_eq!(t.rank_of(&probe), o.rank_of(&probe), "step {step}: rank_of({probe})");
        let r = rng.below(o.0.len().max(1) as u64 + 2) as usize;
        assert_eq!(t.select(r), o.0.get(r), "step {step}: select({r})");
    }
    assert_eq!(t.select(o.0.len()), None, "step {step}: select(len)");
    // iter_from at a random rank must equal the oracle suffix.
    let from = rng.below(o.0.len() as u64 + 2) as usize;
    let got: Vec<u64> = t.iter_from(from).copied().collect();
    let want: Vec<u64> = o.0.iter().skip(from).copied().collect();
    assert_eq!(got, want, "step {step}: iter_from({from})");
    // iter_rev_from(r) = the first r keys, descending.
    let thru = rng.below(o.0.len() as u64 + 2) as usize;
    let got: Vec<u64> = t.iter_rev_from(thru).copied().collect();
    let mut want: Vec<u64> = o.0.iter().take(thru).copied().collect();
    want.reverse();
    assert_eq!(got, want, "step {step}: iter_rev_from({thru})");
}

/// Compare range/count answers over random bound shapes.
fn compare_bounds(step: usize, t: &RankTree<u64>, o: &Oracle, rng: &mut SplitMix) {
    let (a, b) = (rng.below(1_024), rng.below(1_024));
    let (lo, hi) = (a.min(b), a.max(b));
    let shapes: [(Bound<u64>, Bound<u64>); 4] = [
        (Bound::Included(lo), Bound::Included(hi)),
        (Bound::Excluded(lo), Bound::Included(hi)),
        (Bound::Included(lo), Bound::Excluded(hi)),
        (Bound::Unbounded, Bound::Excluded(hi)),
    ];
    for bounds in shapes {
        let want: Vec<u64> =
            o.0.iter()
                .copied()
                .filter(|k| match bounds.0 {
                    Bound::Included(l) => *k >= l,
                    Bound::Excluded(l) => *k > l,
                    Bound::Unbounded => true,
                })
                .filter(|k| match bounds.1 {
                    Bound::Included(h) => *k <= h,
                    Bound::Excluded(h) => *k < h,
                    Bound::Unbounded => true,
                })
                .collect();
        let got: Vec<u64> = t.range(&bounds).copied().collect();
        assert_eq!(got, want, "step {step}: range({bounds:?})");
        assert_eq!(t.count_in(&bounds), want.len(), "step {step}: count_in({bounds:?})");
    }
    // partition_point agrees with the oracle's.
    let cut = rng.below(1_024);
    assert_eq!(
        t.partition_point(|k| *k < cut),
        o.0.partition_point(|k| *k < cut),
        "step {step}: partition_point(< {cut})"
    );
}

fn churn(seed: u64, steps: usize, keyspace: u64, read_every: usize) {
    let mut rng = SplitMix(seed);
    let mut t = RankTree::new();
    let mut o = Oracle::default();
    for step in 0..steps {
        let k = rng.below(keyspace);
        if rng.below(3) == 0 {
            assert_eq!(t.remove(&k), o.remove(&k), "step {step}: remove({k}) return");
        } else {
            assert_eq!(t.insert(k), o.insert(k), "step {step}: insert({k}) return");
        }
        if step % read_every == 0 {
            compare_reads(step, &t, &o, &mut rng);
            compare_bounds(step, &t, &o, &mut rng);
        }
    }
    compare_reads(steps, &t, &o, &mut rng);
    compare_bounds(steps, &t, &o, &mut rng);
}

#[test]
fn dense_keyspace_heavy_collisions() {
    // Small keyspace ⇒ constant duplicate inserts and hit removes; the tree
    // stays a few levels deep and every rebalance shape fires repeatedly.
    churn(0xDEAD_BEEF, 12_000, 512, 97);
}

#[test]
fn wide_keyspace_grows_deep() {
    // Large keyspace ⇒ mostly-growing tree, multiple split levels.
    churn(0x0BAD_CAFE, 12_000, 1_024 * 1_024, 251);
}

#[test]
fn drain_to_empty_and_refill() {
    let mut rng = SplitMix(42);
    let mut t = RankTree::new();
    let mut o = Oracle::default();
    for round in 0..3 {
        for _ in 0..3_000 {
            let k = rng.below(4_096);
            assert_eq!(t.insert(k), o.insert(k));
        }
        compare_reads(round, &t, &o, &mut rng);
        // Remove every live key in a shuffled-ish order via rank probes.
        while !o.0.is_empty() {
            let r = rng.below(o.0.len() as u64) as usize;
            let k = o.0[r];
            assert!(t.remove(&k));
            assert!(o.remove(&k));
        }
        assert!(t.is_empty());
        assert_eq!(t.iter().next(), None);
        assert_eq!(t.select(0), None);
    }
}

#[test]
fn clone_is_deep_and_independent() {
    let mut t = RankTree::new();
    for i in 0..1_000u64 {
        t.insert(i * 3);
    }
    let snap = t.clone();
    for i in 0..500u64 {
        t.remove(&(i * 6));
    }
    assert_eq!(snap.len(), 1_000, "snapshot must not see later removals");
    assert_eq!(t.len(), 500);
    assert_eq!(snap.rank_of(&996), Some(332));
}
