//! kevy-map baseline: get hit / get miss / steady-state insert, at 10k and
//! 1M keys. Keys are byte strings (`key:NNNNNNNN`), the production shape.

use kevy_bench::{bench, black_box};
use kevy_map::KevyMap;

fn keys(prefix: &str, n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("{prefix}:{i:08}").into_bytes()).collect()
}

fn tier(n: usize, samples: usize, get_inner: usize, insert_samples: usize) {
    let hit = keys("key", n);
    let miss = keys("nix", n);

    let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(n);
    for (i, k) in hit.iter().enumerate() {
        m.insert(k.clone(), i as u64);
    }

    let s = bench(samples, get_inner, || {
        for k in &hit {
            black_box(m.get(black_box(k.as_slice())));
        }
    });
    crate::row(&format!("get hit      n={n}"), s, n);

    let s = bench(samples, get_inner, || {
        for k in &miss {
            black_box(m.get(black_box(k.as_slice())));
        }
    });
    crate::row(&format!("get miss     n={n}"), s, n);

    // Steady-state insert: capacity pre-reserved, so no rehash — the cost of
    // the insert path itself (includes the key `clone`, as in bench_vs_std).
    let s = bench(insert_samples, 1, || {
        let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(n);
        for (i, k) in hit.iter().enumerate() {
            m.insert(black_box(k.clone()), i as u64);
        }
        black_box(&m);
    });
    crate::row(&format!("insert       n={n} (with_capacity)"), s, n);
}

pub fn run() {
    println!("== kevy-map ==");
    tier(10_000, 30, 20, 30);
    tier(1_000_000, 9, 1, 7);
    println!();
}
