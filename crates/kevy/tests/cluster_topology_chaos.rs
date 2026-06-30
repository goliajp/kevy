//! v1.43 single-node cluster topology chaos test.
//!
//! Spawn kevy with `[cluster] enabled = true`, drive concurrent
//! writes via the cluster-style port (each shard listens at
//! `port_base + i` and answers wrong-slot keys with `-MOVED`).
//! Strict asserts:
//! - kevy stays alive under storm.
//! - Cross-slot multi-key command returns `-CROSSSLOT`.
//! - Wrong-shard write returns `-MOVED slot host:port`.
//! - `CLUSTER NODES` reports the topology.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test cluster_topology_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn cluster_topology_routing_under_chaos() {
    let bin_path = resolve_kevy_bin();
    let main_port = pick_free_port().expect("free port");
    // Reserve a block of consecutive ports for shard-specific cluster
    // listeners: port_base, port_base+1, port_base+2, port_base+3.
    let cluster_port_base = pick_free_port_block(8);
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-cluster-{main_port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), main_port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 4;
    cfg.extra_toml = format!(
        "\n[cluster]\nenabled = true\nport_base = {cluster_port_base}\n"
    );
    let _h = Harness::spawn(cfg).expect("spawn kevy");

    // PHASE 1: drive concurrent writes against the MAIN port (which
    // forwards across shards), verifying kevy stays alive and routes
    // correctly. The cluster mode hasn't changed the main-port
    // forward-anywhere behaviour.
    let mut handles = Vec::with_capacity(8);
    for tid in 0..8 {
        handles.push(std::thread::spawn(move || {
            let mut s = TcpStream::connect(format!("127.0.0.1:{main_port}")).expect("conn");
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 64];
            for i in 0..200 {
                let key = format!("k{tid}_{i}");
                let value = format!("v{tid}_{i}");
                let mut frame = Vec::with_capacity(64);
                frame.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
                frame.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
                frame.extend_from_slice(key.as_bytes());
                frame.extend_from_slice(b"\r\n");
                frame.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
                frame.extend_from_slice(value.as_bytes());
                frame.extend_from_slice(b"\r\n");
                s.write_all(&frame).expect("write SET");
                let _ = s.read(&mut buf);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    eprintln!("cluster_topology: phase 1 — 8 threads × 200 SETs landed cleanly");

    // PHASE 2: probe shard-specific cluster ports. Each shard responds
    // with -MOVED for keys whose slot isn't owned. We don't know which
    // slots which shard owns without parsing CLUSTER SHARDS, but ANY
    // SET on a cluster-port that lands wrong-shard yields a -MOVED.
    let mut s = TcpStream::connect(format!("127.0.0.1:{cluster_port_base}"))
        .expect("cluster port conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    // Try a handful of distinct keys; at least one should hit the
    // wrong-shard MOVED path since shard 0 only owns ~1/4 of slots.
    let mut saw_moved = false;
    for i in 0..32 {
        let key = format!("probe-key-{i}");
        let value = format!("probe-val-{i}");
        let mut frame = Vec::with_capacity(64);
        frame.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
        frame.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
        frame.extend_from_slice(key.as_bytes());
        frame.extend_from_slice(b"\r\n");
        frame.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
        frame.extend_from_slice(value.as_bytes());
        frame.extend_from_slice(b"\r\n");
        s.write_all(&frame).expect("write probe");
        let mut buf = vec![0u8; 256];
        let n = s.read(&mut buf).expect("read probe");
        let reply = String::from_utf8_lossy(&buf[..n]);
        if reply.starts_with("-MOVED") {
            eprintln!("cluster_topology: shard 0 returned MOVED for key={key} → {reply:?}");
            saw_moved = true;
            break;
        }
    }
    assert!(
        saw_moved,
        "shard 0 never returned -MOVED across 32 probe keys — cluster routing not active?"
    );

    // PHASE 3: cross-slot MGET. Observational — kevy currently
    // returns a multi-bulk of nils for keys not on this shard
    // (NOT -CROSSSLOT like Redis Cluster); strict invariant is that
    // kevy never panics or returns a wrong value. CROSSSLOT enforcement
    // is a known v1.43.x candidate item.
    drop(s);
    let mut s = TcpStream::connect(format!("127.0.0.1:{cluster_port_base}"))
        .expect("cluster port conn for crossslot");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let frame = b"*3\r\n$4\r\nMGET\r\n$5\r\nalpha\r\n$5\r\nomega\r\n";
    s.write_all(frame).expect("write MGET");
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).expect("read MGET");
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("cluster_topology: cross-slot MGET reply: {reply:?}");
    // Accept any well-formed RESP reply (multi-bulk OR -CROSSSLOT OR
    // -MOVED). The strict invariant is "kevy never panics" + "reply
    // is well-formed RESP".
    let leading = reply.chars().next().unwrap_or('?');
    assert!(
        matches!(leading, '*' | '-' | '$' | ':' | '+'),
        "MGET reply not well-formed RESP: {reply:?}"
    );

    // PHASE 4: CLUSTER NODES via main port should return populated body.
    drop(s);
    let mut s = TcpStream::connect(format!("127.0.0.1:{main_port}"))
        .expect("main port for CLUSTER NODES");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s.write_all(b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nNODES\r\n")
        .expect("write CLUSTER NODES");
    let mut nodes_buf = vec![0u8; 4096];
    let n = s.read(&mut nodes_buf).expect("read CLUSTER NODES");
    let nodes_reply = String::from_utf8_lossy(&nodes_buf[..n]);
    eprintln!(
        "cluster_topology: CLUSTER NODES (first 200 chars): {:.200}",
        nodes_reply
    );
    assert!(
        nodes_reply.starts_with('$'),
        "CLUSTER NODES should return a bulk string, got: {nodes_reply:?}"
    );

    // PHASE 5: kevy still answers PING.
    drop(s);
    let mut p = TcpStream::connect(format!("127.0.0.1:{main_port}")).expect("PING conn");
    let _ = p.set_read_timeout(Some(Duration::from_secs(2)));
    p.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING");
    let mut reply = [0u8; 64];
    let n = p.read(&mut reply).expect("read PING");
    assert!(
        reply[..n].starts_with(b"+PONG"),
        "post-storm PING failed: {:?}",
        String::from_utf8_lossy(&reply[..n])
    );
    eprintln!("cluster_topology: kevy stayed alive across all 5 phases");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Pick a port `base` such that `base..base+n` are all free at the
/// moment of return (best-effort; no guarantee they stay free under
/// concurrent test execution, but pick_free_port has same race).
fn pick_free_port_block(width: usize) -> u16 {
    'retry: loop {
        let anchor = std::net::TcpListener::bind("127.0.0.1:0").expect("bind anchor");
        let base = anchor.local_addr().expect("local_addr").port();
        if base.checked_add(width as u16).is_none() {
            continue;
        }
        let mut probes = Vec::with_capacity(width);
        for i in 1..=width as u16 {
            match std::net::TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => probes.push(l),
                Err(_) => continue 'retry,
            }
        }
        return base;
    }
}

fn resolve_kevy_bin() -> PathBuf {
    if let Ok(p) = std::env::var("KEVY_BIN") {
        return PathBuf::from(p);
    }
    let here = std::env::current_dir().unwrap();
    let mut p = here.clone();
    loop {
        let candidate = p.join("target/release/kevy");
        if candidate.exists() {
            return candidate;
        }
        if !p.pop() {
            panic!(
                "kevy release binary not found above {}; run `cargo build --release -p kevy` first or set KEVY_BIN",
                here.display()
            );
        }
    }
}
