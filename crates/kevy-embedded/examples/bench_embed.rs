//! In-process throughput micro-bench for `kevy_embedded::Store` — the path an
//! embed consumer (e.g. mailrs) actually pays per op: the embedded mutex + the
//! keyspace op + (optionally) an AOF append. No socket, no reactor, no network
//! round-trip — so absolute numbers are much higher than the TCP server bench;
//! they measure the in-process data path, not the wire.
//!
//! Run: `cargo run -p kevy-embedded --example bench_embed --release`
//! Override op count with `KEVY_BENCH_N` (default 2_000_000).

use kevy_embedded::{AppendFsync, Config, Store};
use std::time::Instant;

const KEYS: usize = 256;
const VAL: &[u8] = b"value-payload-16";

fn bench(label: &str, store: &Store, n: usize, keys: &[Vec<u8>]) {
    // Warm the keyspace so GET is all hits and allocations are amortized.
    for k in keys {
        store.set(k, VAL).unwrap();
    }

    let t = Instant::now();
    for i in 0..n {
        store.set(&keys[i % KEYS], VAL).unwrap();
    }
    let set_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let mut hits = 0usize;
    for i in 0..n {
        if store.get(&keys[i % KEYS]).unwrap().is_some() {
            hits += 1;
        }
    }
    let get_s = t.elapsed().as_secs_f64();
    std::hint::black_box(hits);

    println!(
        "[{label:<13}] SET {:>10.0} ops/s   GET {:>10.0} ops/s",
        n as f64 / set_s,
        n as f64 / get_s
    );
}

fn main() {
    let n: usize = std::env::var("KEVY_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    // Keys precomputed outside the timed loop so `format!`/alloc cost isn't
    // attributed to kevy.
    let keys: Vec<Vec<u8>> = (0..KEYS).map(|i| format!("k{i}").into_bytes()).collect();

    println!("kevy-embedded in-process throughput — single thread, n={n}, {KEYS} keys, {}B val", VAL.len());

    let s1 = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    bench("in-memory", &s1, n, &keys);

    let dir2 = std::env::temp_dir().join("kevy_embed_bench_everysec");
    let _ = std::fs::remove_dir_all(&dir2);
    let s2 = Store::open(
        Config::default()
            .with_persist(&dir2)
            .with_ttl_reaper_manual()
            .with_appendfsync(AppendFsync::EverySec),
    )
    .unwrap();
    bench("aof-everysec", &s2, n, &keys);

    let dir3 = std::env::temp_dir().join("kevy_embed_bench_always");
    let _ = std::fs::remove_dir_all(&dir3);
    let s3 = Store::open(
        Config::default()
            .with_persist(&dir3)
            .with_ttl_reaper_manual()
            .with_appendfsync(AppendFsync::Always),
    )
    .unwrap();
    // Always-fsync is one fdatasync per write (no group commit on the embedded
    // single-op path) — fsync-rate-bound, so run far fewer ops to stay bounded.
    bench("aof-always", &s3, (n / 20).max(50_000), &keys);

    drop((s1, s2, s3));
    let _ = std::fs::remove_dir_all(&dir2);
    let _ = std::fs::remove_dir_all(&dir3);
}
