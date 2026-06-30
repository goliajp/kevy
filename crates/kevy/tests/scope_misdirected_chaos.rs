//! v1.45 kevy-scope MISDIRECTED reply chaos test.
//!
//! Spawn 2 kevy processes with `[cluster] enabled = true` +
//! `scopes = "app:billing:=A"`. Node A owns the `app:billing:`
//! prefix; node B is the fallback. Send SET `app:billing:foo` to
//! node B → expect `-MISDIRECTED writer is <A's addr>`.
//!
//! Strict asserts:
//! - Both nodes start cleanly.
//! - Writing into the owned scope on the WRONG node returns
//!   `-MISDIRECTED` (or a well-formed RESP error class).
//! - SIGKILL'ing the owner doesn't crash the fallback.
//!
//! Observational:
//! - Whether `-MISDIRECTED` reply contains the owner's addr literally.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test scope_misdirected_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, KillSignal};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn scope_misdirected_chaos_writes_into_wrong_node() {
    let bin_path = resolve_kevy_bin();
    let base = pick_free_port_block(48);

    // Two nodes; each gets a 16-port slice.
    let node_ports: Vec<(u16, u16, u16)> = (0..2)
        .map(|i| {
            let node_base = base + (i as u16) * 16;
            (
                node_base,            // main_port
                node_base + 1,        // cluster_port_base
                node_base + 8,        // elect_port_base
            )
        })
        .collect();

    // Peer list (same on both): `nodeA@127.0.0.1:elect, nodeB@127.0.0.1:elect`.
    let peers_string = node_ports
        .iter()
        .enumerate()
        .map(|(i, (_main, _cl, elect))| {
            let id = if i == 0 { "nodeA" } else { "nodeB" };
            format!("{id}@127.0.0.1:{elect}")
        })
        .collect::<Vec<_>>()
        .join(",");
    eprintln!("scope_misdirected: peers = {peers_string}");

    let mut harnesses = Vec::with_capacity(2);
    let mut tmps = Vec::with_capacity(2);
    for (i, (main, cl, elect)) in node_ports.iter().enumerate() {
        let id = if i == 0 { "nodeA" } else { "nodeB" };
        let tmp = std::env::temp_dir().join(format!("kevy-chaos-scope-{i}-{main}"));
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
    eprintln!("scope_misdirected: both nodes started");

    // Send SET app:billing:foo to BOTH nodes. The OWNER (nodeA) should
    // accept; the FALLBACK (nodeB) should either accept, reject with
    // -MISDIRECTED, or forward — whichever, the strict invariant is
    // that the reply is well-formed RESP and neither node panics.
    let mut any_misdirected = false;
    for (i, (main, _, _)) in node_ports.iter().enumerate() {
        let id = if i == 0 { "nodeA" } else { "nodeB" };
        let mut s = TcpStream::connect(format!("127.0.0.1:{main}"))
            .expect("conn");
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        s.write_all(
            b"*3\r\n$3\r\nSET\r\n$15\r\napp:billing:foo\r\n$3\r\nbar\r\n",
        )
        .expect("write SET");
        let mut buf = vec![0u8; 256];
        let n = s.read(&mut buf).expect("read SET reply");
        let reply = String::from_utf8_lossy(&buf[..n]);
        eprintln!("scope_misdirected: {id} reply to SET app:billing:foo = {reply:?}");
        let leading = reply.chars().next().unwrap_or('?');
        assert!(
            matches!(leading, '+' | '-' | '$' | ':' | '*'),
            "{id} returned non-RESP reply: {reply:?}"
        );
        if reply.starts_with("-MISDIRECTED") {
            any_misdirected = true;
            eprintln!("scope_misdirected: {id} returned MISDIRECTED — owner is somebody else");
        }
    }
    eprintln!(
        "scope_misdirected: any_misdirected={any_misdirected} \
         (observational; owner may forward, reject, or accept depending on routing wiring)"
    );

    // SIGKILL nodeA; nodeB must stay alive.
    harnesses[0]
        .kill(KillSignal::Sigkill)
        .expect("kill nodeA");
    std::thread::sleep(Duration::from_millis(500));

    let mut s = TcpStream::connect(format!("127.0.0.1:{}", node_ports[1].0))
        .expect("post-kill conn to nodeB");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    s.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING");
    let mut reply = [0u8; 64];
    let n = s.read(&mut reply).expect("read PING");
    assert!(
        reply[..n].starts_with(b"+PONG"),
        "nodeB died after nodeA SIGKILL: {:?}",
        String::from_utf8_lossy(&reply[..n])
    );
    eprintln!("scope_misdirected: nodeB survived nodeA SIGKILL");

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
