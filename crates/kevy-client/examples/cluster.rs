//! Minimal `ClusterClient` walkthrough — the typed, cluster-aware client.
//!
//! `ClusterClient` discovers the shard topology once via `CLUSTER SLOTS`, opens
//! one connection per shard, and routes every key to its CRC16 owner — so no
//! command pays the server-side cross-shard forwarding hop. For the why and the
//! perf numbers see `docs/cluster.md`.
//!
//! First start a cluster-mode server (any shard count ≥ 1):
//!   cargo run -p kevy --bin kevy -- --port 6004 --threads 4 --cluster
//! Its shards then listen at 6005, 6006, 6007, 6008 — connect via any of them
//! as the seed (here 6005); the rest are discovered automatically.
//!
//! Then run this example:
//!   cargo run -p kevy-client --example cluster -- 6005

use kevy_client::ClusterClient;

fn main() -> std::io::Result<()> {
    let seed: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6005);

    // Connect to a seed shard; the full topology is discovered from it.
    let mut cc = ClusterClient::connect("127.0.0.1", seed)?;
    println!("connected — {} shard(s)", cc.shard_count());

    // Each of these keys may live on a different shard; the client routes each
    // to its owner with no -MOVED and no forwarding hop.
    cc.set(b"user:1", b"alice")?;
    cc.set(b"user:2", b"bob")?;
    cc.set(b"user:3", b"carol")?;

    println!("user:1 = {:?}", cc.get(b"user:1")?.map(String::from_utf8));

    // INCR routes by key like any single-key command.
    let n = cc.incr(b"counter")?;
    println!("counter = {n}");

    // DEL/EXISTS take keys that may span shards — routed per key and summed.
    let exists = cc.exists(&[b"user:1", b"user:2", b"missing"])?;
    println!("exists(user:1, user:2, missing) = {exists}"); // 2

    let removed = cc.del(&[b"user:1", b"user:2", b"user:3"])?;
    println!("deleted {removed} keys"); // 3

    // DBSIZE / FLUSHALL are whole-cluster: kevy fans them out server-side, so
    // one call covers every shard.
    println!("dbsize (whole cluster) = {}", cc.dbsize()?);

    Ok(())
}
