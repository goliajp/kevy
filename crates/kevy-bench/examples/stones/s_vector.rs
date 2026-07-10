//! kevy-vector HNSW baseline: build 10k × 128d (default params: M=16,
//! ef_construction=200, cosine) and knn@10 with the default query beam
//! (ef=0 → max(4k, 100)).

use crate::rng::Rng;
use kevy_bench::{bench, black_box};
use kevy_vector::{Hnsw, HnswParams};

const N: usize = 10_000;
const DIM: usize = 128;

fn data() -> (Vec<Vec<u8>>, Vec<Vec<f32>>) {
    let mut rng = Rng::new(0x0f10_a7ed);
    let keys = (0..N).map(|i| format!("v:{i:05}").into_bytes()).collect();
    let vecs = (0..N)
        .map(|_| (0..DIM).map(|_| rng.f32_unit() * 2.0 - 1.0).collect())
        .collect();
    (keys, vecs)
}

pub fn run() {
    println!("== kevy-vector (HNSW) ==");
    let (keys, vecs) = data();

    // Build cost per insert; `apply` takes an owned vector, so the 512 B
    // clone is part of the measured op (the production caller hands one over
    // the same way).
    let s = bench(5, 1, || {
        let mut g = Hnsw::new(DIM, HnswParams::default());
        for (k, v) in keys.iter().zip(&vecs) {
            g.apply(black_box(k), Some(black_box(v.clone())));
        }
        black_box(g.stats());
    });
    crate::row("insert 10k × 128d (per insert)", s, N);

    let mut g = Hnsw::new(DIM, HnswParams::default());
    for (k, v) in keys.iter().zip(&vecs) {
        g.apply(k, Some(v.clone()));
    }
    let mut rng = Rng::new(0x05ea_9c40);
    let queries: Vec<Vec<f32>> = (0..100)
        .map(|_| (0..DIM).map(|_| rng.f32_unit() * 2.0 - 1.0).collect())
        .collect();
    let mut qi = 0usize;
    let s = bench(30, 50, || {
        let q = &queries[qi];
        qi = (qi + 1) % queries.len();
        black_box(g.knn(black_box(q), 10, 0));
    });
    crate::row("knn@10 (ef default, 10k graph)", s, 1);
    println!();
}
