//! Tests for [`crate::hnsw`] (child module via `#[path]`, so the
//! graph internals stay reachable).

use crate::dist::Distance;
use super::*;

fn grid(n: usize) -> Hnsw {
    // 2-d grid points — L2 neighbors are unambiguous
    let mut h = Hnsw::new(2, HnswParams { distance: Distance::L2, ..Default::default() });
    for i in 0..n {
        let (x, y) = ((i % 32) as f32, (i / 32) as f32);
        h.apply(format!("p{i:04}").as_bytes(), Some(vec![x, y]));
    }
    h
}

#[test]
fn knn_exact_on_grid() {
    let h = grid(1024);
    // nearest to (5.1, 7.05) is p(7*32+5)=p0229, then p0197/p0230…
    let hits = h.knn(&[5.1, 7.05], 3, 0);
    assert_eq!(hits[0].0, b"p0229".to_vec(), "{hits:?}");
    assert_eq!(hits.len(), 3);
    assert!(hits[0].1 <= hits[1].1);
}

#[test]
fn tombstone_and_replace() {
    let mut h = grid(256);
    h.apply(b"p0000", None);
    assert!(!h.contains(b"p0000"));
    let hits = h.knn(&[0.0, 0.0], 1, 0);
    assert_ne!(hits[0].0, b"p0000".to_vec(), "dead filtered");
    // replace moves the point
    h.apply(b"p0001", Some(vec![100.0, 100.0]));
    let hits = h.knn(&[100.0, 100.0], 1, 0);
    assert_eq!(hits[0].0, b"p0001".to_vec());
    let st = h.stats();
    assert_eq!(st.vectors, 255);
    assert_eq!(st.tombstones, 2, "one delete + one replace");
}

#[test]
fn recall_on_random_vectors() {
    // deterministic pseudo-random 64-d vectors; HNSW top-10 vs
    // brute force ground truth, recall must be ≥ 0.9
    let mut seed = 42u64;
    let mut rnd = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64) as f32 - 0.5
    };
    let mut h = Hnsw::new(64, HnswParams::default());
    let mut all: Vec<(Vec<u8>, Vec<f32>)> = Vec::new();
    for i in 0..2000 {
        let v: Vec<f32> = (0..64).map(|_| rnd()).collect();
        let key = format!("v{i:04}").into_bytes();
        h.apply(&key, Some(v.clone()));
        all.push((key, v));
    }
    let mut hit = 0usize;
    let mut total = 0usize;
    for qi in 0..20 {
        let q: Vec<f32> = (0..64).map(|_| rnd()).collect();
        let got: Vec<Vec<u8>> = h.knn(&q, 10, 0).into_iter().map(|(k, _)| k).collect();
        // brute force with the same metric incl. normalization
        let mut qq = q.clone();
        Distance::Cosine.prepare(&mut qq);
        let mut truth: Vec<(f32, &[u8])> = all
            .iter()
            .map(|(k, v)| {
                let mut vv = v.clone();
                Distance::Cosine.prepare(&mut vv);
                (Distance::Cosine.eval(&vv, &qq), k.as_slice())
            })
            .collect();
        truth.sort_by(|a, b| a.0.total_cmp(&b.0));
        let want: Vec<&[u8]> = truth[..10].iter().map(|(_, k)| *k).collect();
        for w in &want {
            total += 1;
            if got.iter().any(|g| g == w) {
                hit += 1;
            }
        }
        let _ = qi;
    }
    let recall = hit as f64 / total as f64;
    assert!(recall >= 0.9, "recall {recall}");
}

#[test]
fn rebuild_drops_tombstones_preserves_answers() {
    let mut h = grid(512);
    for i in 0..200 {
        h.apply(format!("p{i:04}").as_bytes(), None);
    }
    assert!(h.stats().rebuild_recommended);
    let before = h.knn(&[20.0, 10.0], 5, 0);
    h.rebuild();
    let st = h.stats();
    assert_eq!(st.tombstones, 0);
    assert_eq!(st.vectors, 312);
    let after = h.knn(&[20.0, 10.0], 5, 0);
    assert_eq!(
        before.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        after.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_keys_collapse_onto_one_node() {
    // Two keys, one exact vector → one graph node, two living
    // "vectors"; knn answers with both keys at the same distance.
    let mut h = Hnsw::new(2, HnswParams { distance: Distance::L2, ..Default::default() });
    h.apply(b"a", Some(vec![1.0, 1.0]));
    h.apply(b"b", Some(vec![1.0, 1.0]));
    h.apply(b"c", Some(vec![5.0, 5.0]));
    let st = h.stats();
    assert_eq!(st.vectors, 3);
    assert_eq!(st.tombstones, 0);
    let hits = h.knn(&[1.0, 1.0], 2, 0);
    let mut keys: Vec<&[u8]> = hits.iter().map(|(k, _)| k.as_slice()).collect();
    keys.sort();
    assert_eq!(keys, vec![b"a".as_slice(), b"b".as_slice()]);
    // Detaching one key keeps the shared node alive for the other.
    h.apply(b"a", None);
    assert!(!h.contains(b"a"));
    assert!(h.contains(b"b"));
    assert_eq!(h.stats().vectors, 2);
    assert_eq!(h.stats().tombstones, 0, "node still keyed by b");
    assert_eq!(h.knn(&[1.0, 1.0], 1, 0)[0].0, b"b".to_vec());
    // Detaching the last key tombstones the node.
    h.apply(b"b", None);
    assert_eq!(h.stats().tombstones, 1);
    // Re-adding the vector under a new key revives via a fresh node.
    h.apply(b"d", Some(vec![1.0, 1.0]));
    assert_eq!(h.knn(&[1.0, 1.0], 1, 0)[0].0, b"d".to_vec());
    h.rebuild();
    assert_eq!(h.stats().tombstones, 0);
    assert_eq!(h.stats().vectors, 2);
    assert_eq!(h.knn(&[1.0, 1.0], 1, 0)[0].0, b"d".to_vec());
}

#[test]
fn duplicate_heavy_corpus_stays_connected() {
    // Fuzz finding (artifact
    // hnsw_ops/minimized-from-5290acb6…): duplicate-heavy corpora
    // used to disconnect the graph — recall@10 with an EXHAUSTIVE
    // beam (ef = 512 ≥ N) dropped to 0.8. One-node-per-key
    // duplicates tie every diversity comparison, so inserts and
    // `shrink` re-prunes degenerated and dropped a duplicate
    // cluster's bridges (and, for clusters larger than the link
    // cap, starved out every bridge slot). Fixed by collapsing
    // exact-duplicate vectors onto one graph node. Mirrors the
    // fuzz harness's recall phase byte-for-byte on the minimized
    // artifact.
    const DATA: &[u8] = &[
        0x01, 0x80, 0x08, 0x10, 0x18, 0x20, 0x28, 0xff, 0x16, 0x40, 0x48, 0x50, 0x58,
        0x1c, 0x2d, 0x06, 0x3d, 0x28, 0x29, 0x06, 0x3d, 0x28, 0x2b, 0x2e, 0x31, 0x3d,
        0x85, 0x88, 0x8b, 0x2b, 0x2e, 0x31, 0x34, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
        0x04, 0xca, 0xcd, 0xd0, 0xd3, 0xd6, 0xd9, 0x91, 0x94, 0x2e, 0xca, 0xcd, 0xd0,
        0xd3, 0xd6, 0xd9, 0xdc, 0xdf, 0xe2, 0xe5, 0xe8, 0xeb, 0xee,
    ];
    const DIM: usize = 4;
    fn comp(b: u8) -> f32 {
        f32::from(b as i8) / 8.0
    }
    fn vec_at(off: usize) -> Vec<f32> {
        (0..DIM).map(|j| comp(DATA[(off + j) % DATA.len()])).collect()
    }
    fn l2sq(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }
    let n = DATA.len(); // 62 points, many duplicates (quantized comps)
    let params = HnswParams { m: 8, ef_construction: 64, distance: Distance::L2 };
    let mut h = Hnsw::new(DIM, params);
    let mut points: Vec<(Vec<u8>, Vec<f32>)> = Vec::new();
    for i in 0..n {
        let key = format!("v{i:03}").into_bytes();
        let v = vec_at(i * DIM);
        h.apply(&key, Some(v.clone()));
        points.push((key, v));
    }
    let q = vec_at(n * DIM + 1);
    let got = h.knn(&q, 10, 512);
    assert_eq!(got.len(), 10, "beam lost living points: {got:?}");
    let mut truth: Vec<f32> = points.iter().map(|(_, v)| l2sq(&q, v)).collect();
    truth.sort_by(f32::total_cmp);
    let kth = truth[got.len() - 1];
    let hits = got
        .iter()
        .filter(|(key, _)| {
            let v = &points.iter().find(|(k, _)| k == key).expect("living key").1;
            l2sq(&q, v) <= kth + 1e-4
        })
        .count();
    let recall = hits as f64 / got.len() as f64;
    assert!(recall >= 0.9, "recall@10 {recall} < 0.9 with an exhaustive beam");
}

#[test]
fn empty_and_dim_mismatch() {
    let h = Hnsw::new(4, HnswParams::default());
    assert!(h.knn(&[1.0, 2.0, 3.0, 4.0], 5, 0).is_empty());
    let mut h = grid(16);
    h.apply(b"bad", Some(vec![1.0, 2.0, 3.0])); // wrong dim ignored
    assert!(!h.contains(b"bad"));
    assert!(h.knn(&[1.0], 5, 0).is_empty(), "query dim mismatch");
}

/// v4.1-V5: `stats()` reads running link/tombstone counters instead of
/// walking every node per call. Inserts (with shrink churn), replaces,
/// removals and a rebuild hold them to the walking reference.
#[test]
fn running_stats_never_drift_from_the_walking_reference() {
    let mut g = Hnsw::new(4, HnswParams { m: 4, ef_construction: 32, distance: Distance::L2 });
    let check = |g: &Hnsw, at: &str| {
        assert_eq!(g.stats(), g.recompute_stats(), "counter drift after {at}");
    };
    // Enough inserts that layer-0 caps force shrink pruning.
    for i in 0..120u32 {
        let v: Vec<f32> = (0..4).map(|d| ((i * 31 + d * 7) % 97) as f32).collect();
        g.apply(format!("k{i}").as_bytes(), Some(v));
        if i % 17 == 0 {
            check(&g, &format!("insert {i}"));
        }
    }
    check(&g, "all inserts");
    // Replacements (detach + reinsert) and removals (tombstones).
    for i in (0..120u32).step_by(3) {
        g.apply(format!("k{i}").as_bytes(), None);
    }
    check(&g, "removals");
    for i in (1..60u32).step_by(3) {
        let v: Vec<f32> = (0..4).map(|d| ((i * 13 + d) % 89) as f32 + 0.5).collect();
        g.apply(format!("k{i}").as_bytes(), Some(v));
    }
    check(&g, "replacements");
    g.rebuild();
    check(&g, "rebuild");
    assert_eq!(g.stats().tombstones, 0, "rebuild drops tombstones");
}
