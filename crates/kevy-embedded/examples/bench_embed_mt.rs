//! Multi-threaded in-process throughput for `kevy_embedded::Store` — measures
//! how the embedded keyspace scales across cores. An embed consumer (mailrs is
//! a multi-threaded web server) shares one `Store` across request threads;
//! this bench clones the `Store` (cheap Arc bump → same inner) into T threads
//! all hammering GET / SET, and reports aggregate ops/s at T = 1..N.
//!
//! Run: `cargo run -p kevy-embedded --example bench_embed_mt --release`
//! (pin to disjoint cores, e.g. `taskset -c 0-9`). `KEVY_BENCH_N` = ops/thread.

use kevy_embedded::{Config, Store};
use std::thread;
use std::time::Instant;

const KEYS: usize = 4096;
const VAL: &[u8] = b"value-payload-16";

fn run(label: &str, store: &Store, threads: usize, n_per: usize, keys: &[Vec<u8>], write: bool) {
    let start = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|tid| {
            let s = store.clone(); // Arc bump → same inner / same lock
            let keys = keys.to_vec();
            thread::spawn(move || {
                let mut acc = 0usize;
                for i in 0..n_per {
                    // De-correlate threads' key streams so they don't all hit
                    // the same key/bucket in lockstep.
                    let k = &keys[(i.wrapping_mul(31).wrapping_add(tid * 7)) % KEYS];
                    if write {
                        s.set(k, VAL).unwrap();
                        acc += 1;
                    } else if s.get(k).unwrap().is_some() {
                        acc += 1;
                    }
                }
                acc
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let total = (threads * n_per) as f64;
    println!(
        "[{label:<10}] threads={threads:2}  {:>10.0} ops/s  ({:>5.1}M)",
        total / secs,
        total / secs / 1e6
    );
}

fn main() {
    let n_per: usize = std::env::var("KEVY_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let keys: Vec<Vec<u8>> = (0..KEYS).map(|i| format!("k{i}").into_bytes()).collect();
    let shards: usize = std::env::var("KEVY_SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    // Shared in-memory store (no AOF) — isolate the lock/keyspace from disk.
    let store = Store::open(Config::default().with_shards(shards).with_ttl_reaper_manual()).unwrap();
    for k in &keys {
        store.set(k, VAL).unwrap();
    }

    println!(
        "kevy-embedded MULTI-THREAD throughput — in-memory, shards={shards}, {KEYS} keys, {}B val, n={n_per}/thread",
        VAL.len()
    );
    for &t in &[1usize, 2, 4, 8, 10] {
        run("GET", &store, t, n_per, &keys, false);
    }
    for &t in &[1usize, 2, 4, 8, 10] {
        run("SET", &store, t, n_per, &keys, true);
    }
}
