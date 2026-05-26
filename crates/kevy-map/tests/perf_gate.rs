//! Hot-path budget gate for KevyMap. Asserts per-op ns stays under a
//! documented ceiling. Use kevy-bench for measurement; the SmallBytes-key
//! case is the kevy-store shape (the actual production caller).
//!
//! Reproducer: `cargo test -p kevy-map --test perf_gate --release`

use kevy_bench::{bench, black_box};
use kevy_map::KevyMap;

const KEYS: usize = 1024;

fn keys(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("key:{i:08}").into_bytes()).collect()
}

/// Insert ×KEYS into a fresh table. Per-cmd budget ~ 200 ns conservative
/// (cache-hot ~ 50 ns; loaded host doubles or triples).
#[test]
fn insert_under_budget() {
    let ks = keys(KEYS);
    let s = bench(30, 200, || {
        let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(KEYS);
        for (i, k) in ks.iter().enumerate() {
            m.insert(black_box(k.clone()), i as u64);
        }
        black_box(m);
    });
    // Median is across KEYS inserts per inner-iter; convert to per-insert.
    let per_insert = s.median_ns / KEYS as u64;
    assert!(
        per_insert < 200,
        "KevyMap per-insert median = {per_insert} ns (KEYS={KEYS}), budget 200"
    );
}

/// Warm get hot path. Cache-hot single-key budget should be ~ 30 ns; we set
/// 80 ns to survive loaded-host noise.
#[test]
fn get_hit_under_budget() {
    let ks = keys(KEYS);
    let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(KEYS);
    for (i, k) in ks.iter().enumerate() {
        m.insert(k.clone(), i as u64);
    }
    let s = bench(30, 500, || {
        for k in &ks {
            black_box(m.get(black_box(k.as_slice())));
        }
    });
    let per_get = s.median_ns / KEYS as u64;
    assert!(
        per_get < 80,
        "KevyMap per-get median = {per_get} ns (KEYS={KEYS}), budget 80"
    );
}

/// Remove hot path. Slightly higher budget than get (writes metadata).
#[test]
fn remove_under_budget() {
    let ks = keys(KEYS);
    let s = bench(30, 200, || {
        let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(KEYS);
        for (i, k) in ks.iter().enumerate() {
            m.insert(k.clone(), i as u64);
        }
        for k in &ks {
            black_box(m.remove(black_box(k.as_slice())));
        }
    });
    // s.median_ns covers (insert + remove) × KEYS; isolate via subtraction
    // would over-engineer this test — use a generous combined budget.
    let per_op = s.median_ns / (2 * KEYS) as u64;
    assert!(
        per_op < 250,
        "KevyMap insert+remove combined per-op median = {per_op} ns, budget 250"
    );
}
