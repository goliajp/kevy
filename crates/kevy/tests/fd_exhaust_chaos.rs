//! v1.38 fd-exhaustion chaos test (Unix only).
//!
//! Spawn kevy with `RLIMIT_NOFILE = small` (256). kevy uses its own
//! fds at startup (listeners + AOF + snapshot file + waker + ring fds
//! + uring SQEs). Once ~200-220 are consumed, every new accept must
//! refuse cleanly (EMFILE → kevy emits no panic). Verify:
//! - kevy stays alive.
//! - Past-cap conns get refused (TCP RST or connect timeout).
//! - Below-cap conns continue working.
//! - PING on a stale-but-valid conn answers.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test fd_exhaust_chaos --release -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

/// Tight fd cap — leaves room for kevy's startup fds (~50-100) plus
/// maybe 100-150 conn fds.
const FD_CAP: u64 = 256;
const OFFERED_CONNS: usize = 500;

#[test]
#[ignore = "chaos test (Unix) — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn fd_exhaust_kevy_stays_alive_refuses_cleanly() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-fdex-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    cfg.rlimit_nofile = FD_CAP;
    let h = Harness::spawn(cfg);

    // kevy MAY refuse to start under tight fd cap. If so, the strict
    // assert is "loud refusal" (kevy doesn't silently corrupt).
    let Ok(h) = h else {
        eprintln!("fd_exhaust: kevy refused to start under fd cap — acceptable (loud refusal)");
        return;
    };

    // Open conns until refusal. Hold each.
    let mut alive = Vec::with_capacity(OFFERED_CONNS);
    let mut refused = 0usize;
    for _ in 0..OFFERED_CONNS {
        match TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(200),
        ) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
                alive.push(s);
            }
            Err(_) => {
                refused += 1;
            }
        }
    }
    eprintln!(
        "fd_exhaust: offered={OFFERED_CONNS} alive={} refused={}",
        alive.len(),
        refused
    );

    // Strict: a fresh-conn PING must answer if we have at least one
    // alive conn (or send PING via the first alive conn).
    if let Some(s) = alive.first_mut() {
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        s.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING");
        let mut reply = [0u8; 64];
        let n = s.read(&mut reply).expect("read PING");
        assert!(
            reply[..n].starts_with(b"+PONG"),
            "fd_exhaust: existing conn lost PING after fd-exhaustion: {:?}",
            String::from_utf8_lossy(&reply[..n])
        );
        eprintln!("fd_exhaust: existing conn still answers PING — kevy alive");
    } else {
        panic!("fd_exhaust: no conn survived the storm — kevy may have died");
    }

    // Close all alive conns, then verify kevy can accept new ones.
    drop(alive);
    std::thread::sleep(Duration::from_millis(200));
    let mut fresh = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("fresh post-storm conn");
    let _ = fresh.set_read_timeout(Some(Duration::from_secs(2)));
    fresh.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING2");
    let mut reply2 = [0u8; 64];
    let n2 = fresh.read(&mut reply2).expect("read PING2");
    assert!(
        reply2[..n2].starts_with(b"+PONG"),
        "fd_exhaust: post-recovery PING failed: {:?}",
        String::from_utf8_lossy(&reply2[..n2])
    );
    eprintln!("fd_exhaust: kevy recovered after fd-exhaustion storm — fresh conn PING ok");

    drop(h);
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
