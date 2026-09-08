//! Recall against what it costs, which is the axis this crate has been
//! missing.
//!
//! `Hnsw::knn` carries a note — "measured on a dense 20k cluster @128d:
//! ef 64 → 0.67 recall@10, 100 → 0.77" — and nothing followed it up.
//! It could not be followed up: the only cost axis available was `ef`,
//! and `ef` is a knob, not a cost. Two graphs answering the same query
//! at the same `ef` can differ several-fold in distance computations,
//! so a recall number plotted against `ef` cannot be compared with
//! another implementation, another corpus, or the same code after a
//! change.
//!
//! This plots recall@10 against **distance computations per query**,
//! which is what a search actually spends. Every design question in this
//! crate is a question about that curve: does the neighbour backfill
//! earn the degree it takes, do tombstones eat the beam, did a layout
//! change pay. None of them can be settled against `ef`.
//!
//! Run with the counter compiled in:
//!
//! ```text
//! cargo run --release -p kevy-vector --features count-distances \
//!   --example recall_vs_cost
//! ```
//!
//! Two corpora, because they answer different questions. Uniform random
//! vectors are the easy case and say whether the implementation is
//! sound. Clustered vectors are the case the note above was measured on
//! and the case a real embedding corpus resembles — recall collapses
//! there first, and it collapses for reasons (a pruning heuristic that
//! is not the paper's, a beam spent on redundant neighbours) that the
//! easy corpus cannot show.

use kevy_vector::{Distance, Hnsw, HnswParams};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn unit(&mut self) -> f32 {
        self.next() as f32 / (1u64 << 31) as f32 - 1.0
    }
}

/// `n` vectors of `dim` dimensions, in `clusters` tight groups.
/// `clusters == n` is uniform random.
fn corpus(n: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut r = Lcg(seed);
    let centres: Vec<Vec<f32>> =
        (0..clusters).map(|_| (0..dim).map(|_| r.unit()).collect()).collect();
    (0..n)
        .map(|i| {
            let c = &centres[i % clusters];
            c.iter().map(|&x| x + r.unit() * 0.15).collect()
        })
        .collect()
}

fn brute_force(vecs: &[Vec<f32>], q: &[f32], k: usize, d: Distance) -> Vec<usize> {
    let mut all: Vec<(f32, usize)> =
        vecs.iter().enumerate().map(|(i, v)| (eval_prepared(d, v, q), i)).collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    all.into_iter().take(k).map(|(_, i)| i).collect()
}

/// Cosine on already-normalised vectors, which is what the index stores.
fn eval_prepared(d: Distance, a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    match d {
        Distance::Cosine | Distance::Ip => 1.0 - dot,
        Distance::L2 => a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum(),
    }
}

fn normalise(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

fn main() {
    const N: usize = 20_000;
    const DIM: usize = 128;
    const K: usize = 10;
    const QUERIES: usize = 200;

    for (label, clusters) in [("uniform random", N), ("20 tight clusters", 20)] {
        let mut vecs = corpus(N, DIM, clusters, 7);
        for v in &mut vecs {
            normalise(v);
        }
        let mut idx = Hnsw::new(DIM, HnswParams::default());
        for (i, v) in vecs.iter().enumerate() {
            idx.apply(format!("k{i}").as_bytes(), Some(v.clone()));
        }
        let mut queries = corpus(QUERIES, DIM, clusters.min(QUERIES), 99);
        for q in &mut queries {
            normalise(q);
        }
        let truth: Vec<Vec<usize>> =
            queries.iter().map(|q| brute_force(&vecs, q, K, Distance::Cosine)).collect();

        println!("\n{label}  ({N} x {DIM}d, M=16, efC=200, recall@{K})");
        println!("     ef   recall   distances/query");
        for ef in [16usize, 32, 64, 100, 200, 400] {
            let _ = kevy_vector::count::take();
            let mut hit = 0usize;
            for (q, want) in queries.iter().zip(&truth) {
                let got = idx.knn(q, K, ef);
                let want_keys: Vec<String> = want.iter().map(|&i| format!("k{i}")).collect();
                hit += got
                    .iter()
                    .filter(|(k, _)| want_keys.iter().any(|w| w.as_bytes() == k.as_slice()))
                    .count();
            }
            let dists = kevy_vector::count::take();
            println!(
                "  {ef:5}   {:.3}   {:>9}",
                hit as f64 / (QUERIES * K) as f64,
                dists / QUERIES as u64
            );
        }
    }
    println!(
        "\nThe note in Hnsw::knn records 0.77 recall@10 at ef=100 on a dense\n\
             20k cluster @128d. The clustered rows above are the comparable ones."
    );
}
