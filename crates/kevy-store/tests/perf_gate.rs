//! Regression budgets for the keyspace hot path.
//!
//! These run in the **dev** profile (`cargo test`), which is unoptimised —
//! release is ~5–25× faster — and on a shared host that may be loaded. The
//! budgets therefore carry large headroom on purpose: they exist to catch
//! *order-of-magnitude* regressions (a hasher swap gone wrong, an accidental
//! O(n) probe walk, a per-op allocation that shouldn't be there), not to police
//! single-nanosecond drift. Tighten only with a documented reason.

#![cfg(not(debug_assertions))]

use std::hint::black_box;
use std::time::Duration;

use kevy_bench::time_median;
use kevy_store::Store;

const N: usize = 10_000;
const ITERS: usize = 2_000;
const PAYLOAD: &[u8] = b"value-payload-16";

fn populated() -> (Store, Vec<Vec<u8>>) {
    let mut s = Store::new();
    let keys: Vec<Vec<u8>> = (0..N).map(|i| format!("key:{i:08}").into_bytes()).collect();
    for k in &keys {
        s.set(k, PAYLOAD.to_vec(), None, false, false);
    }
    (s, keys)
}

#[test]
fn get_hit_under_budget() {
    let (mut s, keys) = populated();
    let mut i = 0usize;
    let median = time_median(ITERS, || {
        let k = &keys[i % N];
        i += 1;
        black_box(s.get(k)).ok();
    });
    // Budget 10 µs (dev hit ~150–600 ns; release ~15 ns). Headroom ~15–60×.
    let budget = Duration::from_micros(10);
    assert!(median < budget, "get_hit median {median:?} > {budget:?}");
}

#[test]
fn get_miss_under_budget() {
    let (mut s, _keys) = populated();
    let absent: Vec<Vec<u8>> = (0..N).map(|i| format!("absent:{i:08}").into_bytes()).collect();
    let mut i = 0usize;
    let median = time_median(ITERS, || {
        let k = &absent[i % N];
        i += 1;
        black_box(s.get(k)).ok();
    });
    let budget = Duration::from_micros(10);
    assert!(median < budget, "get_miss median {median:?} > {budget:?}");
}

#[test]
fn set_overwrite_under_budget() {
    let (mut s, keys) = populated();
    let mut i = 0usize;
    let median = time_median(ITERS, || {
        let k = &keys[i % N];
        i += 1;
        s.set(k, PAYLOAD.to_vec(), None, false, false);
    });
    // Budget 20 µs (SET clones the key + boxes the value; dev ~300 ns–1 µs).
    let budget = Duration::from_micros(20);
    assert!(median < budget, "set_overwrite median {median:?} > {budget:?}");
}

#[test]
fn incr_under_budget() {
    // Read-modify-write via `live_entry_mut`: one lookup, mutate in place.
    let mut s = Store::new();
    let keys: Vec<Vec<u8>> = (0..N).map(|i| format!("key:{i:08}").into_bytes()).collect();
    for k in &keys {
        s.set(k, b"0".to_vec(), None, false, false);
    }
    let mut i = 0usize;
    let median = time_median(ITERS, || {
        let k = &keys[i % N];
        i += 1;
        let _ = s.incr_by(k, 1);
    });
    // Budget 20 µs (release ~80 ns; dev ~400 ns–1.5 µs).
    let budget = Duration::from_micros(20);
    assert!(median < budget, "incr median {median:?} > {budget:?}");
}
