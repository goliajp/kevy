//! v1.55 — verify the v1.45.x finding fix: `-MISDIRECTED writer is
//! <host:client_port>` when the peer entry uses the extended
//! `id@host:elect_port:client_port` syntax.
//!
//! Strict asserts:
//! - Both nodes start cleanly with the extended peer syntax.
//! - SET on the non-owner returns `-MISDIRECTED writer is
//!   127.0.0.1:<main_port>` — the CLIENT port, NOT the elect port.
//! - The owner's main port appears in the reply text; the owner's
//!   elect port does NOT appear (regression guard).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test scope_misdirected_client_port --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn scope_misdirected_reply_uses_client_port() {
    let bin_path = resolve_kevy_bin();
    let base = pick_free_port_block(48);

    let node_ports: Vec<(u16, u16, u16)> = (0..2)
        .map(|i| {
            let node_base = base + (i as u16) * 16;
            (node_base, node_base + 1, node_base + 8)
        })
        .collect();

    // v1.55 EXTENDED peer syntax: `id@host:elect_port:client_port`.
    // This is what changes — client_port goes in the MISDIRECTED reply.
    let peers_string = node_ports
        .iter()
        .enumerate()
        .map(|(i, (main, _cl, elect))| {
            let id = if i == 0 { "nodeA" } else { "nodeB" };
            format!("{id}@127.0.0.1:{elect}:{main}")
        })
        .collect::<Vec<_>>()
        .join(",");
    eprintln!("scope_client_port: peers = {peers_string}");

    let mut harnesses = Vec::with_capacity(2);
    let mut tmps = Vec::with_capacity(2);
    for (i, (main, cl, elect)) in node_ports.iter().enumerate() {
        let id = if i == 0 { "nodeA" } else { "nodeB" };
        let tmp = std::env::temp_dir().join(format!("kevy-chaos-scope-cp-{i}-{main}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut cfg = HarnessConfig::new(tmp.clone(), *main).with_fsync("everysec");
        cfg.kevy_bin = bin_path.clone();
        cfg.threads = 2;
        cfg.extra_toml = format!(
            "\n[cluster]\nenabled = true\nport_base = {cl}\nnode_id = \"{id}\"\n\
             elect_port_base = {elect}\npeers = \"{peers_string}\"\n\
             scopes = \"app:billing:=nodeA\"\n"
        );
        harnesses.push(
            Harness::spawn(cfg)
                .unwrap_or_else(|e| panic!("spawn {id} failed: {e}")),
        );
        tmps.push(tmp);
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    eprintln!("scope_client_port: both nodes started");

    // SET on nodeB (the non-owner). Expect MISDIRECTED with nodeA's
    // CLIENT port (== node_ports[0].0), not nodeA's elect port
    // (== node_ports[0].2).
    let nodeb_port = node_ports[1].0;
    let nodea_main = node_ports[0].0;
    let nodea_elect = node_ports[0].2;
    let mut s = TcpStream::connect(format!("127.0.0.1:{nodeb_port}"))
        .expect("conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s.write_all(
        b"*3\r\n$3\r\nSET\r\n$15\r\napp:billing:foo\r\n$3\r\nbar\r\n",
    )
    .expect("write SET");
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).expect("read SET reply");
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("scope_client_port: nodeB SET reply = {reply:?}");

    assert!(
        reply.starts_with("-MISDIRECTED"),
        "expected MISDIRECTED, got: {reply:?}"
    );
    let expected_main = format!("127.0.0.1:{nodea_main}");
    let elect_str = format!("127.0.0.1:{nodea_elect}");
    assert!(
        reply.contains(&expected_main),
        "MISDIRECTED reply missing nodeA main port {nodea_main}: {reply:?}"
    );
    assert!(
        !reply.contains(&elect_str),
        "MISDIRECTED reply leaked nodeA elect port {nodea_elect}: {reply:?}"
    );
    eprintln!(
        "scope_client_port: MISDIRECTED correctly reports CLIENT port {nodea_main}, not elect port {nodea_elect}"
    );

    drop(s);
    drop(harnesses);
    for tmp in tmps {
        let _ = std::fs::remove_dir_all(&tmp);
    }
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
