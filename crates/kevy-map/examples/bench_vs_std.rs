//! KevyMap vs std::HashMap (with FxBuildHasher) micro-bench.
//!
//! The kevy production caller uses `KevyMap<SmallBytes, V>`, but the
//! single-trust-domain Fx-hashed std HashMap is the prior art baseline this
//! crate replaced.
//!
//! `cargo run -p kevy-map --example bench_vs_std --release`

use kevy_bench::{bench, black_box};
use kevy_hash::FxBuildHasher;
use kevy_map::KevyMap;
use std::collections::HashMap;

const SAMPLES: usize = 30;
const INNER: usize = 500;

fn keys(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("key:{i:08}").into_bytes()).collect()
}

fn main() {
    println!("kevy-map vs std::HashMap (FxBuildHasher) micro-bench");
    println!("byte-string keys; ratios are the signal, absolutes drift on a loaded host\n");

    for &n in &[256usize, 4096, 65_536] {
        println!("== n = {n} keys ==");
        let ks = keys(n);

        let km_insert = bench(SAMPLES, INNER, || {
            let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(n);
            for (i, k) in ks.iter().enumerate() {
                m.insert(black_box(k.clone()), i as u64);
            }
            black_box(m);
        });
        let std_insert = bench(SAMPLES, INNER, || {
            let mut m: HashMap<Vec<u8>, u64, FxBuildHasher> =
                HashMap::with_capacity_and_hasher(n, FxBuildHasher::default());
            for (i, k) in ks.iter().enumerate() {
                m.insert(black_box(k.clone()), i as u64);
            }
            black_box(m);
        });
        let per_kev = km_insert.median_ns / n as u64;
        let per_std = std_insert.median_ns / n as u64;
        println!(
            "  insert  KevyMap={per_kev} ns/op  std+Fx={per_std} ns/op  ratio={:.2}×",
            per_std as f64 / per_kev as f64
        );

        let mut km = KevyMap::<Vec<u8>, u64>::with_capacity(n);
        let mut sh: HashMap<Vec<u8>, u64, FxBuildHasher> =
            HashMap::with_capacity_and_hasher(n, FxBuildHasher::default());
        for (i, k) in ks.iter().enumerate() {
            km.insert(k.clone(), i as u64);
            sh.insert(k.clone(), i as u64);
        }
        let km_get = bench(SAMPLES, INNER, || {
            for k in &ks {
                black_box(km.get(black_box(k.as_slice())));
            }
        });
        let std_get = bench(SAMPLES, INNER, || {
            for k in &ks {
                black_box(sh.get(black_box(k.as_slice())));
            }
        });
        let per_kev = km_get.median_ns / n as u64;
        let per_std = std_get.median_ns / n as u64;
        println!(
            "  get-hit KevyMap={per_kev} ns/op  std+Fx={per_std} ns/op  ratio={:.2}×\n",
            per_std as f64 / per_kev as f64
        );
    }
}
