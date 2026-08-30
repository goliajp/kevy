//! kevy-vlog micro-bench — append throughput + read_at latency, the two
//! numbers the tiering trains budget against (spill cost / cold-read cost).
//!
//!   cargo run --release -p kevy-vlog --example bench_vlog

use std::time::Instant;

use kevy_vlog::{DEFAULT_ROTATE_BYTES, Vlog, VlogRef};

fn bench(label: &str, payload: usize, n: u32) {
    let dir = std::env::temp_dir().join(format!("kevy-vlog-bench-{}", std::process::id()));
    let mut v = Vlog::open(&dir, DEFAULT_ROTATE_BYTES).unwrap();
    let val = vec![0xABu8; payload];

    let t0 = Instant::now();
    let refs: Vec<VlogRef> =
        (0..n).map(|i| v.append(format!("key:{i:08}").as_bytes(), &val).unwrap()).collect();
    let append = t0.elapsed();

    let t1 = Instant::now();
    let mut sink = 0usize;
    for r in &refs {
        let (_, p) = v.read(*r).unwrap();
        sink += p.len();
    }
    let read = t1.elapsed();
    assert_eq!(sink, payload * n as usize);

    println!(
        "{label:>8}: append {:>7.2} µs/op ({:>7.1} MB/s) | read_at {:>7.2} µs/op",
        append.as_micros() as f64 / f64::from(n),
        (payload as u64 * u64::from(n)) as f64 / append.as_secs_f64() / 1e6,
        read.as_micros() as f64 / f64::from(n),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    for (label, payload, n) in [
        ("64 B", 64, 100_000),
        ("1 KiB", 1024, 100_000),
        ("4 KiB", 4096, 50_000),
        ("64 KiB", 65536, 5_000),
    ] {
        bench(label, payload, n);
    }
}
