//! v1.57 — verify v1.44.x finding fix: `CLUSTER INFO
//! cluster_known_nodes` reports peer count, not shard count.
//!
//! Strict asserts:
//! - Single-node cluster (no `peers = ...`) reports
//!   `cluster_known_nodes:1` (this node only).
//! - 3-peer cluster reports `cluster_known_nodes:3` (all peers).
//! - `cluster_size` continues to report shard count (Redis spec).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test cluster_known_nodes_count --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn cluster_info_known_nodes_reports_peer_count() {
    let bin_path = resolve_kevy_bin();

    // PHASE 1: single-node cluster — no peers = "..." string, no
    // elect_port_base (matches cluster_topology_chaos minimal config).
    // Expect `cluster_known_nodes:1` (this node only).
    let base_a = pick_free_port_block(16);
    let port_a = base_a;
    let cluster_a = base_a + 1;
    let tmp_a = std::env::temp_dir().join(format!("kevy-chaos-known-a-{port_a}"));
    let _ = std::fs::remove_dir_all(&tmp_a);
    let mut cfg = HarnessConfig::new(tmp_a.clone(), port_a).with_fsync("everysec");
    cfg.kevy_bin = bin_path.clone();
    cfg.threads = 2;
    cfg.extra_toml = format!(
        "\n[cluster]\nenabled = true\nport_base = {cluster_a}\n"
    );
    let h_a = Harness::spawn(cfg).expect("spawn nodeA");
    std::thread::sleep(Duration::from_millis(200));

    let info = query_cluster_info(port_a);
    eprintln!("known_nodes: single-node CLUSTER INFO:\n{info}");
    assert!(
        info.contains("cluster_known_nodes:1\r\n"),
        "single-node expected cluster_known_nodes:1, got: {info:?}"
    );
    assert!(
        info.contains("cluster_size:2\r\n"),
        "single-node expected cluster_size:2 (threads=2), got: {info:?}"
    );
    drop(h_a);
    let _ = std::fs::remove_dir_all(&tmp_a);

    // PHASE 2: 3-peer cluster. Spawn one node configured with a
    // 3-peer list (all listed at fictitious ports; we don't actually
    // start the other 2 — we just verify the node's CLUSTER INFO
    // reads the peer count from its OWN config, not live state).
    let base_b = pick_free_port_block(32);
    let port_b = base_b;
    let cluster_b = base_b + 1;
    let elect_b = base_b + 16;
    let tmp_b = std::env::temp_dir().join(format!("kevy-chaos-known-b-{port_b}"));
    let _ = std::fs::remove_dir_all(&tmp_b);
    let peer_string =
        format!("nodeA@127.0.0.1:{elect_b},nodeB@127.0.0.1:9971,nodeC@127.0.0.1:9981");
    let mut cfg = HarnessConfig::new(tmp_b.clone(), port_b).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 4;
    cfg.extra_toml = format!(
        "\n[cluster]\nenabled = true\nport_base = {cluster_b}\n\
         node_id = \"nodeA\"\nelect_port_base = {elect_b}\n\
         peers = \"{peer_string}\"\n"
    );
    let h_b = Harness::spawn(cfg).expect("spawn 3-peer node");
    std::thread::sleep(Duration::from_millis(200));

    let info = query_cluster_info(port_b);
    eprintln!("known_nodes: 3-peer CLUSTER INFO:\n{info}");
    assert!(
        info.contains("cluster_known_nodes:3\r\n"),
        "3-peer expected cluster_known_nodes:3, got: {info:?}"
    );
    assert!(
        info.contains("cluster_size:4\r\n"),
        "3-peer expected cluster_size:4 (threads=4), got: {info:?}"
    );
    eprintln!("known_nodes: both invariants OK (1 + 3 peers)");
    drop(h_b);
    let _ = std::fs::remove_dir_all(&tmp_b);
}

fn query_cluster_info(port: u16) -> String {
    let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("conn for CLUSTER INFO");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s.write_all(b"*2\r\n$7\r\nCLUSTER\r\n$4\r\nINFO\r\n")
        .expect("write CLUSTER INFO");
    let mut buf = vec![0u8; 4 * 1024];
    let n = s.read(&mut buf).expect("read CLUSTER INFO");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

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
