//! Cross-competitor Rust hashmap bench for kevy-map.
//!
//! Competitors:
//! - `kevy-map::KevyMap`  (the stone)
//! - `hashbrown::HashMap` — the Swiss table behind `std::HashMap`
//! - `std::HashMap` (SipHash; for reference)
//! - `std::HashMap + kevy-hash::FxBuildHasher` (closest std + custom hash)
//!
//! Workloads on 256 / 4 096 / 65 536 byte-string entries:
//! insert (whole-table fill, ns/op) and get_hit (whole-table lookup, ns/op).

use hashbrown::HashMap as HBMap;
use kevy_hash::FxBuildHasher;
use kevy_map::KevyMap;
use rustc_hash::FxBuildHasher as RustcFxBuildHasher;
use std::collections::HashMap as StdMap;
use std::hint::black_box;
use std::time::Instant;

const SAMPLES: usize = 15;
const HOST: &str = "M4-Pro-aarch64";
const STONE: &str = "kevy-map";

fn now_iso() -> String {
    std::process::Command::new("date")
        .args(["-u", "-Iseconds"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn percentiles(times: &mut Vec<u64>) -> (u64, u64, u64) {
    times.sort_unstable();
    let n = times.len();
    (times[n / 2], times[(n * 95) / 100], times[0])
}

fn emit(competitor: &str, workload: &str, m: u64, p95: u64, min: u64, iters: usize) {
    println!(
        "{{\"stone\":\"{STONE}\",\"language\":\"rust\",\"competitor\":\"{competitor}\",\"workload\":\"{workload}\",\"metric\":\"ns_per_op\",\"value_median\":{m},\"value_p95\":{p95},\"value_min\":{min},\"iterations\":{iters},\"host\":\"{HOST}\",\"date\":\"{}\"}}",
        now_iso()
    );
}

fn make_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("session:{i:08}:user").into_bytes())
        .collect()
}

// One-shot timing for the whole-table insert phase. Builds a fresh map per
// sample (since insert mutates state). `build` returns the populated map so
// we can drop it after measuring.
fn bench_insert<M, B: FnMut(&[Vec<u8>]) -> M>(
    competitor: &str,
    n: usize,
    mut build: B,
) {
    let keys = make_keys(n);
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let m = build(&keys);
        let ns = t.elapsed().as_nanos() as u64;
        black_box(m);
        times.push(ns / n as u64);
    }
    let (m, p95, min) = percentiles(&mut times);
    emit(competitor, &format!("insert_n{n}_bytes_key"), m, p95, min, n);
}

// Get-hit timing: build once, then time whole-table lookups SAMPLES times.
// Returns ns/lookup.
fn bench_get_hit<M, B, G>(
    competitor: &str,
    n: usize,
    build: B,
    mut get_once: G,
) where
    B: FnOnce(&[Vec<u8>]) -> M,
    G: FnMut(&M, &[u8]) -> bool,
{
    let keys = make_keys(n);
    let m = build(&keys);
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        for k in &keys {
            black_box(get_once(&m, black_box(k)));
        }
        let ns = t.elapsed().as_nanos() as u64;
        times.push(ns / n as u64);
    }
    let (med, p95, min) = percentiles(&mut times);
    emit(
        competitor,
        &format!("get_hit_n{n}_bytes_key"),
        med,
        p95,
        min,
        n,
    );
    black_box(m);
}

fn main() {
    for &n in &[256usize, 4_096, 65_536] {
        // ---- insert ----
        bench_insert("kevy-map", n, |ks| {
            let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(n);
            for (i, k) in ks.iter().enumerate() {
                m.insert(k.clone(), i as u64);
            }
            m
        });
        bench_insert("hashbrown (ahash)", n, |ks| {
            let mut m: HBMap<Vec<u8>, u64> = HBMap::with_capacity(n);
            for (i, k) in ks.iter().enumerate() {
                m.insert(k.clone(), i as u64);
            }
            m
        });
        bench_insert("hashbrown + rustc-hash", n, |ks| {
            let mut m: HBMap<Vec<u8>, u64, RustcFxBuildHasher> =
                HBMap::with_capacity_and_hasher(n, RustcFxBuildHasher::default());
            for (i, k) in ks.iter().enumerate() {
                m.insert(k.clone(), i as u64);
            }
            m
        });
        bench_insert("std::HashMap (SipHash)", n, |ks| {
            let mut m: StdMap<Vec<u8>, u64> = StdMap::with_capacity(n);
            for (i, k) in ks.iter().enumerate() {
                m.insert(k.clone(), i as u64);
            }
            m
        });
        bench_insert("std::HashMap + kevy-hash", n, |ks| {
            let mut m: StdMap<Vec<u8>, u64, FxBuildHasher> =
                StdMap::with_capacity_and_hasher(n, FxBuildHasher::default());
            for (i, k) in ks.iter().enumerate() {
                m.insert(k.clone(), i as u64);
            }
            m
        });

        // ---- get_hit ----
        bench_get_hit(
            "kevy-map",
            n,
            |ks| {
                let mut m = KevyMap::<Vec<u8>, u64>::with_capacity(n);
                for (i, k) in ks.iter().enumerate() {
                    m.insert(k.clone(), i as u64);
                }
                m
            },
            |m, k| m.get(k).is_some(),
        );
        bench_get_hit(
            "hashbrown (ahash)",
            n,
            |ks| {
                let mut m: HBMap<Vec<u8>, u64> = HBMap::with_capacity(n);
                for (i, k) in ks.iter().enumerate() {
                    m.insert(k.clone(), i as u64);
                }
                m
            },
            |m, k| m.get(k).is_some(),
        );
        bench_get_hit(
            "hashbrown + rustc-hash",
            n,
            |ks| {
                let mut m: HBMap<Vec<u8>, u64, RustcFxBuildHasher> =
                    HBMap::with_capacity_and_hasher(n, RustcFxBuildHasher::default());
                for (i, k) in ks.iter().enumerate() {
                    m.insert(k.clone(), i as u64);
                }
                m
            },
            |m, k| m.get(k).is_some(),
        );
        bench_get_hit(
            "std::HashMap (SipHash)",
            n,
            |ks| {
                let mut m: StdMap<Vec<u8>, u64> = StdMap::with_capacity(n);
                for (i, k) in ks.iter().enumerate() {
                    m.insert(k.clone(), i as u64);
                }
                m
            },
            |m, k| m.get(k).is_some(),
        );
        bench_get_hit(
            "std::HashMap + kevy-hash",
            n,
            |ks| {
                let mut m: StdMap<Vec<u8>, u64, FxBuildHasher> =
                    StdMap::with_capacity_and_hasher(n, FxBuildHasher::default());
                for (i, k) in ks.iter().enumerate() {
                    m.insert(k.clone(), i as u64);
                }
                m
            },
            |m, k| m.get(k).is_some(),
        );
    }
}
