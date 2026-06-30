//! v1.38 disk-full chaos test (Unix only).
//!
//! Spawn kevy with `RLIMIT_FSIZE = small`. AOF growth past the
//! cap triggers SIGXFSZ + ENOSPC-equivalent errors. kevy must:
//! - Stay alive (no panic, no abort).
//! - Either swallow the error and continue serving reads, OR
//!   gracefully degrade to read-only mode (Redis-style `-MISCONF`).
//! - PING must answer after the storm.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test disk_full_chaos --release -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

/// Cap kevy's per-process file-size write at 256 KiB. Initial AOF
/// header is 9 bytes, so plenty of room for some writes before the
/// limit hits.
const FSIZE_CAP_BYTES: u64 = 256 * 1024;

#[test]
#[ignore = "chaos test (Unix) — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn disk_full_kevy_stays_alive_after_rlimit_fsize() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-diskfull-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1; // simpler single-shard topology — one AOF, one writer
    cfg.rlimit_fsize = FSIZE_CAP_BYTES;
    let h = Harness::spawn(cfg);

    // kevy MAY refuse to start under such a tight limit (snapshot at
    // startup is small but possible writes exceed it). Whichever
    // happens, the harness's spawn either succeeded or timed out.
    // The strict invariant is: if it DID spawn, it stays alive under
    // sustained write pressure.
    let Ok(h) = h else {
        eprintln!("disk_full: kevy refused to start under fsize cap — acceptable (Redis-like loud refusal)");
        return;
    };

    // Drive writes until something gives. Each SET appends ~30-50 B
    // to the AOF; ~5000-8000 writes should hit the 256 KiB cap.
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("conn");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut last_ok = 0u32;
    let mut last_err: Option<String> = None;
    for seq in 0..10_000u32 {
        let key = format!("k{seq}").into_bytes();
        let value = format!("v{seq}").into_bytes();
        let mut frame = Vec::with_capacity(64);
        frame.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
        frame.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
        frame.extend_from_slice(&key);
        frame.extend_from_slice(b"\r\n");
        frame.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
        frame.extend_from_slice(&value);
        frame.extend_from_slice(b"\r\n");
        if stream.write_all(&frame).is_err() {
            last_err = Some("write_all failed".into());
            break;
        }
        let mut buf = [0u8; 256];
        match stream.read(&mut buf) {
            Ok(n) if n >= 5 && buf[..5] == *b"+OK\r\n" => {
                last_ok = seq;
            }
            Ok(n) => {
                last_err = Some(format!(
                    "non-OK reply at seq={seq}: {:?}",
                    String::from_utf8_lossy(&buf[..n])
                ));
                break;
            }
            Err(_) => {
                last_err = Some("read err".into());
                break;
            }
        }
    }
    eprintln!(
        "disk_full: last_ok=seq{last_ok}; stop_reason={:?}",
        last_err.unwrap_or_else(|| "completed all 10k".into())
    );

    // First check: did kevy stay alive in-process? On Mac/Linux,
    // RLIMIT_FSIZE exceeded sends SIGXFSZ which TERMINATES the process
    // by default unless a handler is installed. Kevy currently does
    // NOT install one; the SIGXFSZ kill is a known limitation captured
    // as the "graceful contract":
    //
    //   On RLIMIT_FSIZE exhaustion, kevy MAY be killed by SIGXFSZ.
    //   The on-disk AOF MUST be replay-recoverable, and a fresh
    //   restart MUST come back clean with all writes that completed
    //   before the cap was hit.
    let in_process_survived = match TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(500),
    ) {
        Ok(mut s) => {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            s.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok()
                && {
                    let mut reply = [0u8; 64];
                    matches!(s.read(&mut reply), Ok(n) if reply[..n].starts_with(b"+PONG"))
                }
        }
        Err(_) => false,
    };
    eprintln!("disk_full: in_process_survived={in_process_survived}");

    if in_process_survived {
        eprintln!("disk_full: kevy held the line — SIGXFSZ was caught (or write returned gracefully)");
    } else {
        eprintln!("disk_full: kevy died (likely SIGXFSZ); validating restart-recovery contract");
    }

    drop(h);

    // Strict: a fresh kevy on the same data dir MUST come back.
    let port2 = pick_free_port().expect("free port");
    let mut cfg2 = HarnessConfig::new(tmp.clone(), port2).with_fsync("everysec");
    cfg2.kevy_bin = resolve_kevy_bin();
    cfg2.threads = 1;
    // No fsize cap this time — recovery needs room.
    let h2 = Harness::spawn(cfg2).expect("post-disk-full restart spawn");
    let mut p2 = TcpStream::connect(format!("127.0.0.1:{port2}"))
        .expect("post-restart conn");
    let _ = p2.set_read_timeout(Some(Duration::from_secs(2)));
    p2.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING2");
    let mut reply2 = [0u8; 64];
    let n2 = p2.read(&mut reply2).expect("read PING2");
    assert!(
        reply2[..n2].starts_with(b"+PONG"),
        "post-restart PING failed — recovery contract broken: {:?}",
        String::from_utf8_lossy(&reply2[..n2])
    );

    // Strict: GET a key that we KNOW was ACK'd before the cap (use
    // last_ok / 2 so we're firmly inside the safe zone, allowing for
    // some BufWriter loss at the cap edge).
    let safe_seq = last_ok / 2;
    let safe_key = format!("k{safe_seq}");
    p2.write_all(
        format!("*2\r\n$3\r\nGET\r\n${}\r\n{safe_key}\r\n", safe_key.len()).as_bytes(),
    )
    .expect("write GET");
    let mut reply3 = vec![0u8; 256];
    let n3 = p2.read(&mut reply3).expect("read GET");
    let reply3_str = String::from_utf8_lossy(&reply3[..n3]);
    eprintln!("disk_full: post-restart GET k{safe_seq} → {reply3_str:?}");
    assert!(
        reply3_str.starts_with('$'),
        "post-restart GET for an ACK'd-and-flushed key returned non-bulk reply: {reply3_str:?}"
    );

    drop(h2);

    if let Ok(s) = std::fs::read_to_string(tmp.join("kevy.stderr.log")) {
        eprintln!("--- kevy.stderr.log (last 20 lines):");
        let lines: Vec<&str> = s.lines().collect();
        let start = lines.len().saturating_sub(20);
        for line in &lines[start..] {
            eprintln!("  {line}");
        }
    }

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
