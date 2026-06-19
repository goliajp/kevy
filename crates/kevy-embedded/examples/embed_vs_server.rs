//! Unified 4-way bench: same Rust caller, 4 backends.
//!
//! Compares throughput when the **caller is the same Rust code** and
//! only the backend changes:
//!
//! - kevy embed (in-process `kevy_embedded::Store`)
//! - kevy server  via `kevy_client::Connection("kevy://...")`
//! - valkey server via `kevy_client::Connection("redis://...")`
//! - redis server  via `kevy_client::Connection("redis://...")`
//!
//! This is the honest comparison user actually asked for. The server
//! columns are NOT what `redis-benchmark -c1` reports because that's
//! a C client doing zero-alloc argv packing; a Rust caller pays
//! per-call `Vec<Vec<u8>>` allocation, so its server-loopback ceiling
//! sits well below redis-benchmark's. That gap is the point — it's
//! what an actual Rust application sees.
//!
//! Run (against pre-started servers on the documented ports):
//! ```text
//! cargo run -p kevy-embedded --release --example embed_vs_server \
//!   -- --kevy-port 7011 --valkey-port 7012 --redis-port 7013 -N 200000
//! ```
//!
//! Returns a four-column table.

use std::env;
use std::time::Instant;

use kevy_client::Connection;
use kevy_embedded::{Config, Store};

const DEFAULT_N: usize = 200_000;
const VALUE: &[u8] = b"value-payload-16";

#[derive(Clone, Copy)]
struct Args {
    n: usize,
    kevy_port: Option<u16>,
    valkey_port: Option<u16>,
    redis_port: Option<u16>,
    skip_embed: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        n: DEFAULT_N,
        kevy_port: None,
        valkey_port: None,
        redis_port: None,
        skip_embed: false,
    };
    let mut iter = env::args().skip(1);
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "-N" | "--n" => a.n = iter.next().and_then(|s| s.parse().ok()).unwrap_or(a.n),
            "--kevy-port" => a.kevy_port = iter.next().and_then(|s| s.parse().ok()),
            "--valkey-port" => a.valkey_port = iter.next().and_then(|s| s.parse().ok()),
            "--redis-port" => a.redis_port = iter.next().and_then(|s| s.parse().ok()),
            "--no-embed" => a.skip_embed = true,
            "-h" | "--help" => {
                eprintln!(
                    "embed_vs_server [-N <n>] [--kevy-port P] [--valkey-port P] [--redis-port P] [--no-embed]"
                );
                std::process::exit(0);
            }
            _ => eprintln!("unknown flag: {flag}"),
        }
    }
    a
}

fn make_keys(prefix: &str, n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("{prefix}{i}").into_bytes()).collect()
}

fn time_ns<F: FnOnce()>(f: F) -> u128 {
    let t = Instant::now();
    f();
    t.elapsed().as_nanos()
}

fn rate(n: usize, dt_ns: u128) -> f64 {
    (n as f64) * 1_000_000_000.0 / (dt_ns as f64)
}

struct Row {
    label: &'static str,
    set_rps: f64,
    get_rps: f64,
}

fn embed_row(n: usize) -> Row {
    let s = Store::open(Config::default().without_aof()).expect("Store::open");
    let keys = make_keys("k:e:", n);
    // Warm: populate to hit steady-state overwrite + get-hit costs.
    for k in &keys {
        s.set(k, VALUE).expect("warm");
    }
    let dt_set = time_ns(|| {
        for k in &keys {
            let _ = s.set(k, VALUE);
        }
    });
    let dt_get = time_ns(|| {
        for k in &keys {
            let _ = s.get(k);
        }
    });
    Row {
        label: "kevy embed",
        set_rps: rate(n, dt_set),
        get_rps: rate(n, dt_get),
    }
}

fn server_row(label: &'static str, url: &str, n: usize) -> Row {
    let mut conn = match Connection::open(url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  [{label}] open {url} failed: {e}");
            return Row {
                label,
                set_rps: 0.0,
                get_rps: 0.0,
            };
        }
    };
    let keys = make_keys(&format!("k:{label}:"), n);
    for k in &keys {
        let _ = conn.set(k, VALUE);
    }
    let dt_set = time_ns(|| {
        for k in &keys {
            let _ = conn.set(k, VALUE);
        }
    });
    let dt_get = time_ns(|| {
        for k in &keys {
            let _ = conn.get(k);
        }
    });
    Row {
        label,
        set_rps: rate(n, dt_set),
        get_rps: rate(n, dt_get),
    }
}

fn print_table(rows: &[Row]) {
    println!();
    println!(
        "| {:<22} | {:>14} | {:>14} |",
        "backend", "SET ops/s", "GET ops/s"
    );
    println!("|{:-<24}|{:->16}|{:->16}|", "", "", "");
    for r in rows {
        if r.set_rps == 0.0 && r.get_rps == 0.0 {
            println!("| {:<22} | {:>14} | {:>14} |", r.label, "(skipped)", "(skipped)");
        } else {
            println!(
                "| {:<22} | {:>14.0} | {:>14.0} |",
                r.label, r.set_rps, r.get_rps
            );
        }
    }
}

fn main() {
    let a = parse_args();
    println!("unified Rust caller, single-conn sequential, N={n}", n = a.n);
    let mut rows = Vec::new();
    if !a.skip_embed {
        rows.push(embed_row(a.n));
    }
    if let Some(p) = a.kevy_port {
        rows.push(server_row(
            "kevy server (Rust)",
            &format!("kevy://127.0.0.1:{p}"),
            a.n,
        ));
    }
    if let Some(p) = a.valkey_port {
        rows.push(server_row(
            "valkey 9.1 (Rust)",
            &format!("redis://127.0.0.1:{p}"),
            a.n,
        ));
    }
    if let Some(p) = a.redis_port {
        rows.push(server_row(
            "redis 7.4 (Rust)",
            &format!("redis://127.0.0.1:{p}"),
            a.n,
        ));
    }
    print_table(&rows);
}
