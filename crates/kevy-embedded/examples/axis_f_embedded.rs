//! Axis F bench harness — kevy-embedded in-process performance.
//!
//! valkey has NO embedded mode (the entire `valkey-server` binary is a
//! TCP server). This axis showcases kevy's **unique capability** rather
//! than a side-by-side competitor comparison. Output speaks the same
//! shape as `redis-benchmark -q` (op + ops/sec + ns/op) so the numbers
//! line up against the TCP matrix.
//!
//! Run:
//!   cargo run -p kevy-embedded --example axis_f_embedded --release
//!
//! Override op count with `KEVY_BENCH_N` (default 1_000_000).

use kevy_embedded::{Config, Store};
use std::time::Instant;

const N_DEFAULT: usize = 1_000_000;
const KEY_PREFIX: &str = "axis-f:";
const VAL_SMALL: &[u8] = b"v";
const VAL_16: &[u8] = b"value-payload-16";
const VAL_1K: [u8; 1024] = [b'x'; 1024];
const VAL_10K: [u8; 10240] = [b'x'; 10240];

fn ops_per_sec(n: usize, dt_ns: u128) -> f64 {
    (n as f64) * 1_000_000_000.0 / (dt_ns as f64)
}

fn ns_per_op(n: usize, dt_ns: u128) -> f64 {
    (dt_ns as f64) / (n as f64)
}

fn make_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("{KEY_PREFIX}{i:08}").into_bytes()).collect()
}

fn time<F: FnMut()>(mut f: F) -> u128 {
    let t = Instant::now();
    f();
    t.elapsed().as_nanos()
}

fn print_row(label: &str, n: usize, dt: u128) {
    println!(
        "axis_f\tkevye\t{label:<20}\t{:>14.0} ops/s   {:>6.0} ns/op",
        ops_per_sec(n, dt),
        ns_per_op(n, dt)
    );
}

fn main() {
    let n: usize = std::env::var("KEVY_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(N_DEFAULT);
    println!("# axis_f kevy-embedded bench, N = {n}");
    println!("# (valkey + redis have no in-process mode — kevy-unique capability)");
    println!();

    let s = Store::open(Config::default().without_aof()).expect("Store::open");

    // ---- A: trivial ops on a small inline value ----
    let keys = make_keys(n);
    // Warm: populate so SET is overwrite-cost.
    for k in &keys {
        s.set(k, VAL_16).expect("warm set");
    }

    let dt = time(|| {
        for k in &keys {
            let _ = s.set(k, VAL_16);
        }
    });
    print_row("SET 16B (overwrite)", n, dt);

    let dt = time(|| {
        for k in &keys {
            let _ = s.get(k);
        }
    });
    print_row("GET 16B (hit)", n, dt);

    let absent = make_keys(n);
    let absent: Vec<Vec<u8>> = absent
        .into_iter()
        .map(|mut v| {
            v.extend_from_slice(b":miss");
            v
        })
        .collect();
    let dt = time(|| {
        for k in &absent {
            let _ = s.get(k);
        }
    });
    print_row("GET (miss)", n, dt);

    // ---- B: INCR (L2 fast path) ----
    // Re-set keys to "0" so first INCR sees Str then promotes to Int.
    for k in &keys {
        let _ = s.set(k, b"0");
    }
    let dt = time(|| {
        for k in &keys {
            let _ = s.incr_by(k, 1);
        }
    });
    print_row("INCR (Int fast)", n, dt);

    // ---- C: bigger values (L1 ArcBulk path) ----
    for k in &keys {
        let _ = s.set(k, &VAL_1K);
    }
    let dt = time(|| {
        for k in &keys {
            let _ = s.set(k, &VAL_1K);
        }
    });
    print_row("SET 1KB", n, dt);

    let dt = time(|| {
        for k in &keys {
            let _ = s.get(k);
        }
    });
    print_row("GET 1KB", n, dt);

    for k in &keys {
        let _ = s.set(k, &VAL_10K);
    }
    let n10k = n / 10; // 10x bigger value, keep wall-clock comparable
    let dt = time(|| {
        for k in keys.iter().take(n10k) {
            let _ = s.set(k, &VAL_10K);
        }
    });
    print_row("SET 10KB", n10k, dt);

    let dt = time(|| {
        for k in keys.iter().take(n10k) {
            let _ = s.get(k);
        }
    });
    print_row("GET 10KB", n10k, dt);

    // ---- D: DEL ----
    let dt = time(|| {
        for k in &keys {
            let _ = s.del(&[k.as_slice()]);
        }
    });
    print_row("DEL", n, dt);

    // ---- E: tiny single-byte value ----
    for k in keys.iter().take(n / 10) {
        let _ = s.set(k, VAL_SMALL);
    }
    let dt = time(|| {
        for k in keys.iter().take(n / 10) {
            let _ = s.set(k, VAL_SMALL);
        }
    });
    print_row("SET 1B", n / 10, dt);

    let dt = time(|| {
        for k in keys.iter().take(n / 10) {
            let _ = s.get(k);
        }
    });
    print_row("GET 1B", n / 10, dt);

    println!();
    println!("# Headline: kevye is the in-process keyspace ceiling.");
    println!("# vs the TCP matrix: divide by network-loopback c1-P1 (~95 k ops/s).");
    println!("# Implied 'network tax' = kevy-TCP / kevye = how much the wire costs.");
}
