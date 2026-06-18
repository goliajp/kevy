//! Latency/throughput bench for the real `ClusterClient` (vs the raw-socket
//! probe used during the perf investigation): each thread opens its own
//! cluster client (one connection per shard, CRC16 routing) and loops GET over
//! keys spanning all shards. Proves the typed API hits the same client-side-
//! routing ceiling.
//!
//! Run against a cluster server (`kevy --cluster --threads N`):
//!   cargo run --release --example cluster_bench -- <seed_port> <iters> <keys> <conc>
//! `seed_port` = any cluster port (server `port + 1`); the topology is
//! discovered via CLUSTER SLOTS.

use std::time::Instant;

use kevy_client::ClusterClient;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seed: u16 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(7001);
    let iters: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let keys: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let conc: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);

    // Populate.
    {
        let mut cc = ClusterClient::connect("127.0.0.1", seed).unwrap();
        for k in 0..keys {
            cc.set(format!("k{k}").as_bytes(), b"v").unwrap();
        }
    }

    let t0 = Instant::now();
    let handles: Vec<_> = (0..conc)
        .map(|c| {
            std::thread::spawn(move || {
                let mut cc = ClusterClient::connect("127.0.0.1", seed).unwrap();
                let mut lat = Vec::with_capacity(iters);
                for i in 0..iters {
                    let key = format!("k{}", (i * 7 + c * 131) % keys);
                    let t = Instant::now();
                    let _ = cc.get(key.as_bytes()).unwrap();
                    lat.push(t.elapsed().as_nanos() as u64);
                }
                lat
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    let wall = t0.elapsed().as_secs_f64();
    all.sort_unstable();
    let p = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)] as f64 / 1000.0;
    println!(
        "ClusterClient conc={conc} n={} ops/s={:.0} p50={:.1}us p99={:.1}us p999={:.1}us",
        all.len(),
        all.len() as f64 / wall,
        p(0.50),
        p(0.99),
        p(0.999),
    );
}
