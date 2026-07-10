//! Fuzz `kevy_vector::Hnsw` — insert/replace/delete/rebuild/knn sequences
//! must never panic, and small-N recall vs a brute-force oracle must stay
//! ≥ 0.9.
//!
//! Two phases per input (fixed dimension 4, adversarial data driven
//! entirely by the fuzz bytes — duplicate points, collinear clusters,
//! zero vectors under cosine, tombstone-heavy graphs):
//!
//! 1. **Op-sequence totality** across all three metrics with small
//!    input-driven M / ef_construction: apply (insert / replace /
//!    delete), knn (incl. k = 0, ef = 0 default and dim-mismatched
//!    queries), rebuild, stats, contains. Asserted on every knn: results
//!    ascending by distance, ≤ k results, only living keys, no
//!    duplicates. After rebuild: zero tombstones, live count preserved.
//!
//! 2. **Recall@10 vs brute force** (L2, N ≤ 200 from the input bytes,
//!    query beam ef = 512 ≥ N so layer-0 search is effectively
//!    exhaustive over the connected component): with ties handled by
//!    distance threshold (a returned point counts as a hit iff its true
//!    distance ≤ the true 10th-nearest distance + eps), recall must be
//!    ≥ 0.9 and every reported distance must match a recomputed L2
//!    distance. A recall failure here means the graph got disconnected
//!    (select_diverse / shrink dropped a bridge) — a genuine finding,
//!    not noise, because the beam covers the whole component.
//!
//! FIXED FINDING (2026-07-10, fuzz-found within ~3 min): duplicate-
//! heavy corpora used to disconnect the graph — recall@10 came back
//! 0.8 with the exhaustive beam. Mechanism: for a candidate `c`
//! identical to an already kept neighbor `s`, `select_diverse`'s old
//! keep test `d(node,c) < d(s,c) = 0` was always false, so inserts
//! and `shrink()` re-prunes could drop a duplicate cluster's only
//! bridge. Fixed upstream (tie-distance candidates are kept, matching
//! hnswlib's Algorithm-4 heuristic); the recall floor below now runs
//! for EVERY corpus, duplicates included.

#![no_main]

use kevy_vector::{Distance, Hnsw, HnswParams};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

const DIM: usize = 4;

/// Component from one byte: finite, small range, quantized so duplicate
/// and near-duplicate points are common.
fn comp(b: u8) -> f32 {
    f32::from(b as i8) / 8.0
}

fn vec_at(data: &[u8], off: usize) -> Vec<f32> {
    (0..DIM)
        .map(|j| comp(*data.get((off + j) % data.len().max(1)).unwrap_or(&0)))
        .collect()
}

fn l2sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn check_knn_shape(h: &Hnsw, got: &[(Vec<u8>, f32)], k: usize) {
    assert!(got.len() <= k, "knn returned more than k results");
    for w in got.windows(2) {
        assert!(w[0].1 <= w[1].1, "knn results not ascending: {got:?}");
    }
    for (key, _) in got {
        assert!(h.contains(key), "knn returned a dead/absent key");
    }
    for i in 0..got.len() {
        for j in i + 1..got.len() {
            assert_ne!(got[i].0, got[j].0, "knn returned a duplicate key");
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // ---- Phase 1: op-sequence totality across metrics ----
    let metric = match data[0] % 3 {
        0 => Distance::Cosine,
        1 => Distance::L2,
        _ => Distance::Ip,
    };
    let m = 2 + (data[0] >> 2) as usize % 7; // 2..=8: small M stresses pruning
    let params = HnswParams { m, ef_construction: 16 + m * 4, distance: metric };
    let mut h = Hnsw::new(DIM, params);
    assert_eq!(h.dim(), DIM);
    let mut pos = 1usize;
    let mut ops = 0usize;
    while pos < data.len() && ops < 48 {
        let op = data[pos];
        pos += 1;
        ops += 1;
        let id = usize::from(op >> 3) % 24; // small key space → replaces + tombstones
        let key = format!("p{id}").into_bytes();
        match op % 8 {
            0..=3 => h.apply(&key, Some(vec_at(data, pos))),
            4 => h.apply(&key, None),
            5 => {
                let k = usize::from(op >> 4) % 12; // includes k = 0
                let ef = if op & 1 == 0 { 0 } else { 32 };
                let got = h.knn(&vec_at(data, pos), k, ef);
                check_knn_shape(&h, &got, k);
                // dim mismatch must be a clean empty answer
                assert!(h.knn(&[1.0], k, ef).is_empty());
            }
            6 => {
                let before = h.stats().vectors;
                let had = h.contains(&key);
                h.rebuild();
                let st = h.stats();
                assert_eq!(st.tombstones, 0, "rebuild left tombstones");
                assert_eq!(st.vectors, before, "rebuild changed the live count");
                assert_eq!(h.contains(&key), had, "rebuild changed membership");
            }
            7 => {
                let _ = h.stats();
                let _ = h.contains(&key);
            }
            _ => unreachable!(),
        }
    }

    // ---- Phase 2: recall@10 vs brute force (L2, exhaustive beam) ----
    let n = data.len().clamp(8, 200);
    let params = HnswParams { m: 8, ef_construction: 64, distance: Distance::L2 };
    let mut h = Hnsw::new(DIM, params);
    let mut points: BTreeMap<Vec<u8>, Vec<f32>> = BTreeMap::new();
    for i in 0..n {
        let key = format!("v{i:03}").into_bytes();
        let v = vec_at(data, i * DIM);
        h.apply(&key, Some(v.clone()));
        points.insert(key, v);
    }
    let q = vec_at(data, n * DIM + 1);
    let got = h.knn(&q, 10, 512);
    assert_eq!(got.len(), 10.min(n), "knn returned fewer than min(k, N) living results");
    check_knn_shape(&h, &got, 10);

    let mut truth: Vec<f32> = points.values().map(|v| l2sq(&q, v)).collect();
    truth.sort_by(f32::total_cmp);
    let kth = truth[got.len() - 1];
    let eps = 1e-4;
    let mut hits = 0usize;
    for (key, reported) in &got {
        let true_d = l2sq(&q, &points[key]);
        assert!(
            (true_d - reported).abs() <= 1e-3 * true_d.max(1.0),
            "knn reported a wrong distance for {key:?}: {reported} vs {true_d}"
        );
        if true_d <= kth + eps {
            hits += 1;
        }
    }
    let recall = hits as f64 / got.len() as f64;
    assert!(
        recall >= 0.9,
        "recall@10 {recall} < 0.9 with an exhaustive beam — graph disconnected?"
    );
});
