//! v0.metal-1 step 2: the cross-core transport floor. Measures the raw SPSC ring
//! op cost (same-thread push+pop) and the cross-thread throughput / per-item
//! latency — the primitive every cross-shard command rides. Indicative under
//! host load; the cross-thread number is the one that reflects cache-coherency
//! traffic between cores.
//!
//! `cargo run -p kevy-ring --example bench_ring --release`

use kevy_bench::{bench, black_box, report};
use kevy_ring::ring;
use std::time::Instant;

fn main() {
    println!("kevy-ring micro-bench (indicative under host load)\n");

    // Raw ring op cost, no cross-core coherency (one thread).
    let (mut p, mut c) = ring::<u64>(1024);
    report(
        "push+pop u64 (same thread)",
        bench(80, 50_000, || {
            let _ = p.push(black_box(42u64));
            black_box(c.pop());
        }),
    );

    // Cross-thread SPSC: the real cross-core transport floor (head/tail cache
    // lines bounce between the two cores).
    for cap in [256usize, 1024] {
        let n = 50_000_000u64;
        let (mut p, mut c) = ring::<u64>(cap);
        let consumer = std::thread::spawn(move || {
            let mut got = 0u64;
            while got < n {
                if c.pop().is_some() {
                    got += 1;
                }
            }
        });
        let t = Instant::now();
        let mut sent = 0u64;
        while sent < n {
            if p.push(sent).is_ok() {
                sent += 1;
            }
        }
        consumer.join().unwrap();
        let secs = t.elapsed().as_secs_f64();
        println!(
            "cross-thread SPSC cap={cap:<5} {:>6.1}M items/s   {:>5.1} ns/item",
            n as f64 / secs / 1e6,
            secs * 1e9 / n as f64
        );
    }
}
