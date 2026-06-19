//! T4.25 — pipeline example: queue a batch of mixed commands and run
//! them in a single TCP round-trip.
//!
//! ```text
//! cargo run -p kevy-client-async --example pipeline --features tokio
//! ```
//!
//! Pipelines collapse N commands into one write + one read pass.
//! On a sequential workload this is essentially free RTT savings —
//! one trip instead of N. Per-command errors land as
//! `Reply::Error(_)` entries in the returned Vec so a single bad
//! command doesn't tear down the whole batch.

use kevy_client_async::AsyncConnection;
use kevy_resp::Reply;
use std::env;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let url = env::var("KEVY_URL").unwrap_or_else(|_| "tcp://127.0.0.1:6004".into());
    let mut conn = AsyncConnection::open(&url).await?;

    // Mixed batch: writes + reads + counter + cleanup.
    let replies = conn
        .pipeline()
        .set(b"k1", b"first")
        .set(b"k2", b"second")
        .get(b"k1")
        .get(b"k2")
        .incr(b"hits")
        .incr(b"hits")
        .del(&[&b"k1"[..], &b"k2"[..]])
        .run(&mut conn)
        .await?;

    println!("pipeline batch sent — {} replies received:", replies.len());
    for (i, r) in replies.iter().enumerate() {
        match r {
            Reply::Simple(s) => println!("  [{i}] simple {}", String::from_utf8_lossy(s)),
            Reply::Bulk(v) => println!("  [{i}] bulk   {}", String::from_utf8_lossy(v)),
            Reply::Int(n) => println!("  [{i}] int    {n}"),
            Reply::Nil => println!("  [{i}] nil"),
            Reply::Error(e) => println!("  [{i}] ERROR  {}", String::from_utf8_lossy(e)),
            other => println!("  [{i}] other  {other:?}"),
        }
    }

    conn.del(&[&b"hits"[..]]).await?;
    Ok(())
}
