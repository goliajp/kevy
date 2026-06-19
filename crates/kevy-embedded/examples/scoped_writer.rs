//! `kevy-embedded` as a Phase 3 scope writer.
//!
//! Shows the embed-as-writer pattern from v1.21: an application
//! embeds the source-of-truth store for a key prefix, exposes a
//! replication source listener for cluster readers, and writes
//! locally with zero network round-trip on the write path.
//!
//! ## Run
//!
//! ```sh
//! cargo run -p kevy-embedded --example scoped_writer --release
//! ```
//!
//! In another terminal, subscribe with `kevy-cli` or any
//! cluster-aware reader pointed at `127.0.0.1:16204` (the example
//! prints the bound address on startup).

use std::time::Duration;

use kevy_embedded::{Config, Store};

fn main() {
    let listen_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:16204".to_string());

    println!("kevy-embedded scoped-writer demo");
    println!("  replication source listening on {listen_addr}");

    let writer = Store::open(
        Config::default()
            // 1 MiB backlog is plenty for the demo; production
            // scopes size this to cover the worst-case replica
            // disconnect window. See `docs/cluster.md` Phase 3.
            .with_embed_writer_backlog(1024 * 1024)
            .with_embed_writer(&listen_addr),
    )
    .expect("failed to open embed-as-writer store");

    // Local writes go through the standard embed API; every commit
    // additionally pushes into the backlog so subscribers see them.
    println!("  applying initial writes…");
    writer.set(b"app:billing:invoice:42", b"100.00 USD").unwrap();
    writer.set(b"app:billing:invoice:43", b"200.00 USD").unwrap();
    writer
        .hset(
            b"app:billing:customer:7",
            &[(&b"name"[..], &b"Alice"[..]), (&b"plan"[..], &b"pro"[..])],
        )
        .unwrap();

    println!("  store now holds:");
    for key in [
        &b"app:billing:invoice:42"[..],
        b"app:billing:invoice:43",
    ] {
        if let Some(v) = writer.get(key).unwrap() {
            println!(
                "    {:?} = {:?}",
                std::str::from_utf8(key).unwrap(),
                std::str::from_utf8(&v).unwrap(),
            );
        }
    }

    // Subscribe pointing at this listener with any
    // `kevy_replicate::ReplicaClient`-compatible reader to receive
    // these frames (and any subsequent writes) in offset order.
    println!("  sleeping 30 s — connect a reader during this window");
    std::thread::sleep(Duration::from_secs(30));

    println!("  bye");
}
