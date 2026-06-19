//! T4.23 benchmark — async (tokio) vs blocking on the same workload.
//!
//! `#[ignore]` so it doesn't run in `cargo test`. Invoke explicitly:
//!
//! ```text
//! KEVY_URL=tcp://127.0.0.1:6004 \
//!   cargo test -p kevy-client-async --release --features tokio \
//!   --test bench_vs_blocking -- --ignored --nocapture
//! ```
//!
//! Run a kevy server first:
//! ```text
//! cargo run -p kevy --bin kevy -- --port 6004
//! ```
//!
//! Pass-criterion from RFC F5 / plan T4.23:
//! - single-conn async ≥ 80 % blocking throughput
//! - high-concurrency async ≥ blocking throughput
//!
//! We exercise both on the same single-connection workload (8 K
//! sequential SET ops) and report ops/sec for each side; the
//! pipelined variant runs the same 8 K ops in batches of 64.

#![cfg(feature = "tokio")]

use std::time::Instant;

use kevy_client_async::AsyncConnection;

const N: usize = 8_192;
const PIPE: usize = 64;

fn url() -> String {
    std::env::var("KEVY_URL").unwrap_or_else(|_| "tcp://127.0.0.1:6004".to_string())
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn async_sequential() {
    let mut c = AsyncConnection::open(&url())
        .await
        .expect("kevy server reachable; set KEVY_URL or start `cargo run -p kevy`");
    // Warm.
    c.ping().await.unwrap();
    let t = Instant::now();
    for i in 0..N {
        let k = format!("bench:{i}");
        c.set(k.as_bytes(), b"v").await.unwrap();
    }
    let dt = t.elapsed();
    println!(
        "async   sequential SET ×{N}: {:?} ({:.0} ops/s)",
        dt,
        (N as f64) / dt.as_secs_f64()
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn async_pipelined() {
    let mut c = AsyncConnection::open(&url()).await.expect("kevy reachable");
    c.ping().await.unwrap();
    let t = Instant::now();
    let mut written = 0;
    while written < N {
        let take = PIPE.min(N - written);
        let mut p = c.pipeline();
        for j in 0..take {
            let k = format!("bench:{}", written + j);
            // Builder borrows owned bytes via push_raw — clone the
            // key on the heap since the typed setters take &[u8].
            p = p.push_raw(vec![b"SET".to_vec(), k.into_bytes(), b"v".to_vec()]);
        }
        let replies = p.run(&mut c).await.unwrap();
        assert_eq!(replies.len(), take);
        written += take;
    }
    let dt = t.elapsed();
    println!(
        "async   pipelined  SET ×{N} (batch {PIPE}): {:?} ({:.0} ops/s)",
        dt,
        (N as f64) / dt.as_secs_f64()
    );
}

#[test]
#[ignore]
fn blocking_sequential() {
    let mut c = kevy_client::Connection::open(&url())
        .expect("kevy server reachable; set KEVY_URL or start `cargo run -p kevy`");
    c.ping().unwrap();
    let t = Instant::now();
    for i in 0..N {
        let k = format!("bench:{i}");
        c.set(k.as_bytes(), b"v").unwrap();
    }
    let dt = t.elapsed();
    println!(
        "blocking sequential SET ×{N}: {:?} ({:.0} ops/s)",
        dt,
        (N as f64) / dt.as_secs_f64()
    );
}
