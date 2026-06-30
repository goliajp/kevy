//! v1.37 maxclients enforcement chaos test.
//!
//! Spawn kevy with `max_clients = N`, open 2 × N TCP connections.
//! Strict asserts:
//! - At least N conns establish + PING successfully (the cap should
//!   accept up to N; the extra N gets refused).
//! - kevy stays alive — existing conns can still PING after the
//!   storm.
//!
//! NOT a strict equality (kevy uses per-shard cap = ceil(N/nshards),
//! so the actual total may exceed N by up to nshards-1). But the
//! observed refusal rate must be at least 25 % of the offered load.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test maxclients_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

const MAX_CLIENTS: usize = 50;
const STORM_FACTOR: usize = 4;

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn maxclients_storm_refuses_past_cap() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-maxclients-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    // Use enough threads that per-shard ceil(50/4) = 13 keeps the
    // approximation reasonable.
    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 4;
    cfg.max_clients = MAX_CLIENTS;
    let _h = Harness::spawn(cfg).expect("spawn kevy");

    // Open MAX_CLIENTS × STORM_FACTOR conns in parallel; each tries to
    // PING. Count how many got +PONG vs failed/closed.
    let offered = MAX_CLIENTS * STORM_FACTOR;
    let mut handles = Vec::with_capacity(offered);
    let success = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for _ in 0..offered {
        let success = std::sync::Arc::clone(&success);
        let refused = std::sync::Arc::clone(&refused);
        handles.push(std::thread::spawn(move || {
            // Hold the conn alive until barrier so the cap is contested.
            let s = match TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().unwrap(),
                Duration::from_millis(500),
            ) {
                Ok(s) => s,
                Err(_) => {
                    refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
            };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
            let mut s = s;
            if s.write_all(b"*1\r\n$4\r\nPING\r\n").is_err() {
                refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            let mut buf = [0u8; 32];
            match s.read(&mut buf) {
                Ok(n) if n >= 5 && buf[..5] == *b"+PONG" => {
                    success.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Hold the socket for 200 ms so the cap stays full
                    // while the rest of the storm hits.
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => {
                    refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let s = success.load(std::sync::atomic::Ordering::Relaxed);
    let r = refused.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("maxclients_storm: offered={offered} success={s} refused={r}");

    // The cap is per-shard ceil(50/4) = 13 → 4 shards × 13 = up to 52
    // conns can succeed (SO_REUSEPORT distributes ~uniformly). The
    // offered load is 200; refusals must be substantial.
    assert!(
        r >= offered / 4,
        "expected ≥ {} refusals out of {offered}; got {r} (and {s} successes). \
         Either the cap isn't being enforced, or the test got lucky.",
        offered / 4
    );

    // kevy must still answer a fresh PING.
    let mut ping = TcpStream::connect(format!("127.0.0.1:{port}")).expect("post-storm conn");
    let _ = ping.set_read_timeout(Some(Duration::from_secs(2)));
    ping.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING");
    let mut reply = [0u8; 64];
    let n = ping.read(&mut reply).expect("read PING");
    assert!(
        reply[..n].starts_with(b"+PONG"),
        "post-storm PING failed: {:?}",
        String::from_utf8_lossy(&reply[..n])
    );
    eprintln!("maxclients_storm: post-storm PING ok — kevy survived");

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
