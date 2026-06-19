//! In-process embedded throughput vs network-loopback servers.
//!
//! Measures the cost difference between `kevy-embedded::Store` (direct
//! method calls, no socket, no parsing) and a TCP-loopback Redis-
//! protocol server. The latter number is what `redis-benchmark -c1`
//! produces against kevy / valkey / redis on the same host.
//!
//! The architectures aren't apples-to-apples — valkey and redis have
//! no in-process mode, so the only fair statement is "embed skips the
//! wire layer; here is how much that saves." The 3-way table in
//! `bench/REPORT.md` lays it out explicitly.
//!
//! Run: `cargo run -p kevy-embedded --example embed_throughput --release`
//!
//! Output is plain ops/sec + p50 ns so it lines up with redis-benchmark's
//! `rps` and `avg_msec` columns.

use std::time::Instant;

use kevy_embedded::{Config, Store};

const N: usize = 1_000_000;
const VALUE: &[u8] = b"value-payload-16";

fn make_keys(prefix: &str, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("{prefix}{i}").into_bytes())
        .collect()
}

fn time_ns<F: FnMut()>(mut f: F) -> u128 {
    let t = Instant::now();
    f();
    t.elapsed().as_nanos()
}

fn ops_per_sec(n: usize, dt_ns: u128) -> f64 {
    (n as f64) * 1_000_000_000.0 / (dt_ns as f64)
}

fn main() {
    let keys = make_keys("k:", N);
    let absent = make_keys("miss:", N);

    let s = Store::open(Config::default().without_aof()).expect("Store::open");

    // Warm: populate the table so SET timing is overwrite-cost (matches
    // what a steady-state server sees), not first-insert + capacity
    // growth.
    for k in &keys {
        s.set(k, VALUE).expect("warm set");
    }

    println!("kevy-embedded in-process throughput, N={N} ops, 16-byte value, 12-byte key");
    println!("(host: {})", hostname());
    println!();

    // SET (overwrite path).
    let dt = time_ns(|| {
        for k in &keys {
            let _ = s.set(k, VALUE);
        }
    });
    let rps = ops_per_sec(N, dt);
    println!(
        "SET   (overwrite) : {rps:>13.0} ops/s  ({:.0} ns/op)",
        (dt as f64) / (N as f64)
    );

    // GET hit.
    let dt = time_ns(|| {
        for k in &keys {
            let _ = s.get(k);
        }
    });
    let rps = ops_per_sec(N, dt);
    println!(
        "GET   (hit)       : {rps:>13.0} ops/s  ({:.0} ns/op)",
        (dt as f64) / (N as f64)
    );

    // GET miss.
    let dt = time_ns(|| {
        for k in &absent {
            let _ = s.get(k);
        }
    });
    let rps = ops_per_sec(N, dt);
    println!(
        "GET   (miss)      : {rps:>13.0} ops/s  ({:.0} ns/op)",
        (dt as f64) / (N as f64)
    );

    // INCR — exercises live_entry_mut + integer fast path.
    for k in &keys {
        let _ = s.set(k, b"0");
    }
    let dt = time_ns(|| {
        for k in &keys {
            let _ = s.incr_by(k, 1);
        }
    });
    let rps = ops_per_sec(N, dt);
    println!(
        "INCR              : {rps:>13.0} ops/s  ({:.0} ns/op)",
        (dt as f64) / (N as f64)
    );

    // DEL.
    let dt = time_ns(|| {
        for k in &keys {
            let _ = s.del(&[k.as_slice()]);
        }
    });
    let rps = ops_per_sec(N, dt);
    println!(
        "DEL               : {rps:>13.0} ops/s  ({:.0} ns/op)",
        (dt as f64) / (N as f64)
    );

    println!();
    println!("Compare against `redis-benchmark -c1 -P1 -n 300000` running");
    println!("against the same host's kevy / valkey / redis server (see");
    println!("bench/loopback_c1.sh).");
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
