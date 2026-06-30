//! v1.46 — network-partition chaos test (client-side).
//!
//! Production networks see TCP resets, half-closed conns, mid-protocol
//! disconnects, and bursts of slowloris-style abandoned connections.
//! This test exercises kevy's resilience to those patterns:
//!
//! - **Burst-abandon**: open 200 conns, send a partial RESP frame,
//!   abruptly drop the socket. kevy must not panic / leak.
//! - **Half-close**: send a valid command, half-close write side,
//!   read the reply, drop. kevy must clean up properly.
//! - **Reconnect storm**: 1000 fast connect → PING → disconnect cycles
//!   from one source. kevy must not accumulate dead conn state.
//! - **Post-storm health**: fresh-conn PING must answer +PONG.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test network_partition_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn network_partition_client_side_disconnects() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-netpart-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let _h = Harness::spawn(cfg).expect("spawn kevy");

    // PHASE 1: burst-abandon — open many conns, write a partial frame
    // each, drop without reading.
    eprintln!("network_partition: phase 1 — burst-abandon 200 conns with partial frames");
    let mut alive_conns = Vec::with_capacity(200);
    for _ in 0..200 {
        if let Ok(mut s) = TcpStream::connect(format!("127.0.0.1:{port}")) {
            let _ = s.set_write_timeout(Some(Duration::from_millis(100)));
            // Write a partial RESP frame — kevy must NOT crash trying
            // to parse it.
            let _ = s.write_all(b"*3\r\n$3\r\nSET\r\n$3\r\nfo");
            alive_conns.push(s);
        }
    }
    eprintln!("network_partition: opened {} conns with partial frames", alive_conns.len());
    // Abrupt drop — destructors send FIN, but with un-acked partial
    // bytes kevy sees these as torn-frame disconnects.
    drop(alive_conns);
    std::thread::sleep(Duration::from_millis(200));

    // PHASE 2: half-close — write a valid command, shutdown write side,
    // read reply, drop.
    eprintln!("network_partition: phase 2 — 50 half-close patterns");
    for _ in 0..50 {
        if let Ok(mut s) = TcpStream::connect(format!("127.0.0.1:{port}")) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = s.write_all(b"*1\r\n$4\r\nPING\r\n");
            let _ = s.shutdown(std::net::Shutdown::Write);
            let mut buf = [0u8; 32];
            let _ = s.read(&mut buf);
        }
    }
    std::thread::sleep(Duration::from_millis(200));

    // PHASE 3: reconnect storm — fast connect → PING → disconnect ×1000.
    eprintln!("network_partition: phase 3 — 1000-conn reconnect storm");
    let storm_start = std::time::Instant::now();
    let mut storm_ok = 0;
    let mut storm_err = 0;
    for _ in 0..1000 {
        match TcpStream::connect(format!("127.0.0.1:{port}")) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                if s.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok() {
                    let mut buf = [0u8; 32];
                    if let Ok(n) = s.read(&mut buf) {
                        if n >= 5 && buf[..5] == *b"+PONG" {
                            storm_ok += 1;
                            continue;
                        }
                    }
                }
                storm_err += 1;
            }
            Err(_) => storm_err += 1,
        }
    }
    let storm_elapsed = storm_start.elapsed();
    eprintln!(
        "network_partition: storm 1000 = {storm_ok} OK / {storm_err} err in {:.2} s",
        storm_elapsed.as_secs_f64()
    );
    // Strict: ≥ 95 % of storm conns must succeed (TIME_WAIT exhaustion
    // is OS-side; kevy itself should NOT be refusing).
    assert!(
        storm_ok * 20 >= 1000 * 19,
        "storm OK rate too low: {storm_ok}/1000 — kevy may be refusing conns"
    );

    // PHASE 4: post-storm health — fresh PING.
    eprintln!("network_partition: phase 4 — fresh-conn PING");
    let mut ping = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("post-storm conn");
    let _ = ping.set_read_timeout(Some(Duration::from_secs(2)));
    ping.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING");
    let mut reply = [0u8; 64];
    let n = ping.read(&mut reply).expect("read PING");
    assert!(
        reply[..n].starts_with(b"+PONG"),
        "post-storm PING failed: {:?}",
        String::from_utf8_lossy(&reply[..n])
    );
    eprintln!("network_partition: kevy alive across all 4 phases");

    let _ = std::fs::remove_dir_all(&tmp);
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
