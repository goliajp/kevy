//! Steady-state throughput bench: async-sequential vs async-pipelined
//! vs blocking-sequential against a live kevy server.
//!
//! Larger N + warmup than the `tests/bench_vs_blocking.rs` ignored
//! tests so connect / runtime / TLS-handshake costs don't dominate
//! the per-op number.
//!
//! Run against a live kevy server:
//! ```text
//! KEVY_URL=tcp://127.0.0.1:6004 \
//!   cargo run -p kevy-client-async --release --features tokio \
//!   --example bench_throughput
//! ```

use std::env;
use std::time::Instant;

use kevy_client_async::AsyncConnection;

const N: usize = 100_000;
const PIPE: usize = 64;
const WARMUP: usize = 10_000;

fn url() -> String {
    env::var("KEVY_URL").unwrap_or_else(|_| "tcp://127.0.0.1:6004".to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = url();
    println!("Target: {url} | N={N} | PIPE={PIPE} | warmup={WARMUP}");

    let mut c = AsyncConnection::open(&url).await.expect("kevy reachable");
    for i in 0..WARMUP {
        let k = format!("bench:warm:{i}");
        c.set(k.as_bytes(), b"v").await.unwrap();
    }

    // async sequential
    let t = Instant::now();
    for i in 0..N {
        let k = format!("bench:a:{i}");
        c.set(k.as_bytes(), b"v").await.unwrap();
    }
    let dt = t.elapsed();
    println!(
        "async   sequential SET ×{N:>7}: {:>8.2} ms  ({:>8.0} ops/s)",
        dt.as_secs_f64() * 1e3,
        (N as f64) / dt.as_secs_f64()
    );

    // async pipelined
    let t = Instant::now();
    let mut written = 0;
    while written < N {
        let take = PIPE.min(N - written);
        let mut p = c.pipeline();
        for j in 0..take {
            let k = format!("bench:p:{}", written + j);
            p = p.push_raw(vec![b"SET".to_vec(), k.into_bytes(), b"v".to_vec()]);
        }
        let replies = p.run(&mut c).await.unwrap();
        assert_eq!(replies.len(), take);
        written += take;
    }
    let dt = t.elapsed();
    println!(
        "async   pipelined  SET ×{N:>7}: {:>8.2} ms  ({:>8.0} ops/s)  batch={PIPE}",
        dt.as_secs_f64() * 1e3,
        (N as f64) / dt.as_secs_f64()
    );

    // blocking sequential — spawn_blocking out of the async task
    let blocking_url = url.clone();
    tokio::task::spawn_blocking(move || {
        let mut bc = kevy_client::Connection::open(&blocking_url).expect("kevy reachable");
        for i in 0..WARMUP {
            let k = format!("bench:warm:{i}");
            bc.set(k.as_bytes(), b"v").unwrap();
        }
        let t = Instant::now();
        for i in 0..N {
            let k = format!("bench:b:{i}");
            bc.set(k.as_bytes(), b"v").unwrap();
        }
        let dt = t.elapsed();
        println!(
            "blocking sequential SET ×{N:>7}: {:>8.2} ms  ({:>8.0} ops/s)",
            dt.as_secs_f64() * 1e3,
            (N as f64) / dt.as_secs_f64()
        );
    })
    .await
    .unwrap();

    println!();
    println!("Headline: pipeline at batch={PIPE} collapses N RTTs to N/{PIPE}.");
    println!("Pure async-vs-blocking single-conn cost should be ≥ 80% blocking (RFC F5).");
}
