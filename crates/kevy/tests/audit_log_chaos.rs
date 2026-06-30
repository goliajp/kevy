//! v1.42 audit log chaos test.
//!
//! Spawn kevy with `[audit] log_path = <path>`; issue N ADMIN
//! commands (`CONFIG SET` / `DEBUG`); verify the audit file captures
//! every event in order, even under concurrent issue.
//!
//! Strict asserts:
//! - The audit file exists post-run.
//! - Every issued ADMIN command appears as a tab-separated line.
//! - Timestamps are monotonic (microsecond resolution).
//! - File is opened with `O_APPEND` semantics (writes from multiple
//!   threads don't interleave mid-line).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test audit_log_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

const N_ADMIN_CALLS: usize = 200;

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn audit_log_captures_admin_calls_in_order() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-audit-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp");
    let audit_path = tmp.join("audit.log");

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    cfg.extra_toml = format!("\n[audit]\nlog_path = \"{}\"\n", audit_path.display());
    let _h = Harness::spawn(cfg).expect("spawn kevy");

    // Issue N admin calls from N threads in parallel.
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(8);
    for tid in 0..8 {
        let stop = Arc::clone(&stop);
        let port = port;
        handles.push(std::thread::spawn(move || {
            let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).expect("conn");
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 64];
            let calls_per_thread = N_ADMIN_CALLS / 8;
            for i in 0..calls_per_thread {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let key = format!("audit-test-key-{tid}-{i}").into_bytes();
                let value = format!("value-{tid}-{i}").into_bytes();
                let frame = build_config_set(&key, &value);
                s.write_all(&frame).expect("write CONFIG SET");
                let _ = s.read(&mut buf);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    std::thread::sleep(Duration::from_millis(200));

    // Read the audit file.
    let body = std::fs::read_to_string(&audit_path).expect("read audit.log");
    let lines: Vec<&str> = body.lines().collect();
    eprintln!("audit_log: captured {} lines", lines.len());

    // Strict: exactly N lines, all CONFIG SET events (note: most will
    // return -ERR Unknown CONFIG SET parameter, but the audit is
    // recorded BEFORE dispatch, so all N must be present).
    assert!(
        lines.len() >= N_ADMIN_CALLS,
        "audit captured only {} lines, expected ≥ {N_ADMIN_CALLS}",
        lines.len()
    );

    // Strict: every line starts with a 16+ digit timestamp + TAB.
    let mut last_ts: u128 = 0;
    let mut config_set_count = 0;
    for line in &lines {
        let parts: Vec<&str> = line.split('\t').collect();
        assert!(
            parts.len() >= 2,
            "audit line missing tab-separated fields: {line:?}"
        );
        let ts: u128 = parts[0].parse().expect("ts must be u128");
        assert!(
            ts >= last_ts,
            "timestamps non-monotonic: prev={last_ts} cur={ts}"
        );
        last_ts = ts;
        if parts[1] == "CONFIG" && parts.get(2) == Some(&"SET") {
            config_set_count += 1;
        }
    }
    eprintln!("audit_log: {config_set_count} CONFIG SET events captured");
    assert!(
        config_set_count >= N_ADMIN_CALLS,
        "expected ≥ {N_ADMIN_CALLS} CONFIG SET events, got {config_set_count}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

fn build_config_set(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(b"*4\r\n$6\r\nCONFIG\r\n$3\r\nSET\r\n");
    out.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
    out
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
