//! SHUTDOWN graceful drain chaos test (Unix).
//!
//! Spawn kevy, hammer it with concurrent writes, issue the `SHUTDOWN`
//! command over a plain client connection. kevy must drain exactly
//! like the SIGTERM path: fsync the AOF tail, land in-flight persist
//! jobs, exit 0. Strict asserts:
//! - The client sees the connection close without a reply.
//! - kevy exits 0 within the drain timeout.
//! - Every primary-ACK'd write is present on a fresh restart on the
//!   same data dir (the drain fsyncs the everysec window).
//! - `SHUTDOWN SAVE` additionally leaves per-shard `dump-{i}.rdb`
//!   snapshots behind.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test shutdown_drain_chaos --release -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, WriterPool, pick_free_port, pipelined_verify_counts};

/// Send `SHUTDOWN [arg]` on a fresh connection; return once the server
/// closes it (EOF) or errors out. Any reply bytes are returned so the
/// caller can assert the no-reply contract.
fn send_shutdown(port: u16, arg: Option<&str>) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect for SHUTDOWN");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let frame = match arg {
        None => b"*1\r\n$8\r\nSHUTDOWN\r\n".to_vec(),
        Some(a) => format!("*2\r\n$8\r\nSHUTDOWN\r\n${}\r\n{}\r\n", a.len(), a).into_bytes(),
    };
    s.write_all(&frame).expect("write SHUTDOWN");
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
    }
    reply
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn shutdown_drains_cleanly_no_lost_writes() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-shutdown-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    // Drive concurrent writes for 2 s, then SHUTDOWN.
    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(port, 4, Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(2));
    let pre = pool.log.lock().unwrap().len();
    assert!(pre >= 100, "vacuous test: only {pre} ACKs before SHUTDOWN");
    eprintln!("shutdown_drain: {pre} ACKs before SHUTDOWN");

    let start = std::time::Instant::now();
    let reply = send_shutdown(port, None);
    assert!(
        reply.is_empty(),
        "SHUTDOWN must not reply before closing; got {:?}",
        String::from_utf8_lossy(&reply)
    );
    let code = h
        .wait_exit(Duration::from_secs(10))
        .expect("wait_exit")
        .expect("kevy did not exit within 10 s of SHUTDOWN");
    let drain_elapsed = start.elapsed();
    assert_eq!(code, 0, "SHUTDOWN drain must exit 0");
    eprintln!("shutdown_drain: exit 0 after {:.2} s", drain_elapsed.as_secs_f64());

    stop.store(true, Ordering::Relaxed);
    let log = pool.join();
    let acks = log.lock().unwrap().clone();

    // Restart on the same data dir; ACK'd writes must survive.
    h.restart().expect("restart");
    let (present, lost, corrupted) = pipelined_verify_counts(port, &acks);
    eprintln!("shutdown_drain: present={present} lost={lost} corrupted={}", corrupted.len());
    assert!(
        corrupted.is_empty(),
        "CORRUPTION DETECTED after SHUTDOWN drain: {}",
        corrupted.join("\n")
    );
    let loss = lost as f64 / (acks.len() as f64).max(1.0);
    assert!(
        loss < 0.01,
        "SHUTDOWN drain lost {:.2} % of writes ({lost}/{}) — graceful contract is broken",
        loss * 100.0,
        acks.len()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn shutdown_save_writes_final_snapshot() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-shutdown-save-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    // A few writes so the snapshot has content, then SHUTDOWN SAVE.
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    for i in 0..50 {
        let key = format!("snap:{i}");
        let frame = format!("*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n$2\r\nok\r\n", key.len(), key);
        s.write_all(frame.as_bytes()).unwrap();
        let mut ack = [0u8; 5];
        s.read_exact(&mut ack).expect("SET ack");
        assert_eq!(&ack, b"+OK\r\n");
    }
    drop(s);

    let reply = send_shutdown(port, Some("SAVE"));
    assert!(reply.is_empty(), "SHUTDOWN SAVE must not reply before closing");
    let code = h
        .wait_exit(Duration::from_secs(10))
        .expect("wait_exit")
        .expect("kevy did not exit within 10 s of SHUTDOWN SAVE");
    assert_eq!(code, 0, "SHUTDOWN SAVE drain must exit 0");

    let dumps: Vec<_> = std::fs::read_dir(&tmp)
        .expect("data dir readable")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("dump-") && n.ends_with(".rdb"))
        .collect();
    assert!(!dumps.is_empty(), "SHUTDOWN SAVE left no dump-*.rdb in {}", tmp.display());
    eprintln!("shutdown_save: final snapshots {dumps:?}");

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
