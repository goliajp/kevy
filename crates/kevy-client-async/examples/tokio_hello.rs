//! T4.24 — minimal tokio-runtime example: open a connection, PING,
//! SET/GET round-trip.
//!
//! Run against a local kevy server:
//! ```text
//! cargo run -p kevy --bin kevy -- --port 6004 &
//! cargo run -p kevy-client-async --example tokio_hello --features tokio
//! ```

use kevy_client_async::AsyncConnection;
use std::env;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let url = env::var("KEVY_URL").unwrap_or_else(|_| "tcp://127.0.0.1:6004".into());
    println!("connecting to {url}");

    let mut conn = AsyncConnection::open(&url).await?;
    conn.ping().await?;
    println!("PING → PONG");

    conn.set(b"hello", b"world").await?;
    let v = conn.get(b"hello").await?;
    println!(
        "SET hello world / GET hello → {:?}",
        v.as_deref().map(|b| String::from_utf8_lossy(b))
    );

    conn.del(&[&b"hello"[..]]).await?;
    println!("DEL hello");

    Ok(())
}
