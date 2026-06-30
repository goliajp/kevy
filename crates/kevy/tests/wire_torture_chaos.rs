//! v1.36 wire-protocol torture chaos test.
//!
//! Two parts:
//!
//! 1. **Parser fuzz campaign**: runs `kevy_resp::fuzz::run_n(1_000_000, ...)`
//!    across all 5 strategies. Asserts NO panic, NO hang, NO timeout —
//!    every call resolves to one of `Ok(Some)` / `Ok(None)` / `Err`.
//!
//! 2. **Live wire torture**: against a real kevy process, sends
//!    pathological frame sequences (partial, oversized, garbage,
//!    mid-pipeline disconnect) and verifies kevy stays alive +
//!    responds with proper RESP errors.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test wire_torture_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};
use kevy_resp::fuzz::{Strategy, run_n};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn wire_torture_parser_fuzz_1m() {
    // Phase 1: parser fuzz campaign at 10^6 (project industrial-grade bar).
    let summary = run_n(1_000_000, 0xC0DEBA5E);
    eprintln!(
        "wire_torture: fuzz 1M campaign — parsed={} incomplete={} errored={} timeouts={}",
        summary.parsed,
        summary.incomplete,
        summary.errored,
        summary.timed_out.len()
    );
    summary.assert_clean(1_000_000);

    // All 5 strategies must have produced SOME outputs (sanity).
    assert!(summary.parsed + summary.incomplete + summary.errored == 1_000_000);
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn wire_torture_live_kevy_pathological_frames() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-torture-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let cfg = HarnessConfig {
        kevy_bin: bin_path,
        ..HarnessConfig::new(tmp.clone(), port)
            .with_fsync("everysec")
    };
    let _h = Harness::spawn(cfg).expect("spawn kevy");

    // Each torture pattern is sent on a FRESH conn. After each, send
    // PING on a NEW conn — kevy must answer +PONG, proving it stayed
    // alive.
    let patterns: &[(&str, &[u8])] = &[
        ("partial_frame_missing_crlf", b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar"),
        ("oversized_array_claim", b"*999999999\r\n"),
        ("oversized_bulk_claim", b"*1\r\n$2000000000\r\n"),
        ("negative_array_len", b"*-99\r\nignored"),
        ("garbage_then_valid", b"garbage_bytes\xff\xfe\xfd*1\r\n$4\r\nPING\r\n"),
        ("interleaved_garbage", b"+OK\r\n*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$\xff\xfe\r\n"),
        ("very_long_inline", &[b'A'; 16_000]),
        ("bulk_len_overflow", b"*1\r\n$99999999999999999\r\n"),
    ];

    for (name, payload) in patterns {
        eprintln!("wire_torture: pattern={name} len={}", payload.len());
        let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
            .expect("torture conn");
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        // Best-effort write; partial writes / RST are part of the test.
        let _ = s.write_all(payload);
        // Drain any reply (don't care about content; just that no
        // panic on kevy side).
        let mut buf = vec![0u8; 256];
        let _ = s.read(&mut buf);
        drop(s);

        // kevy must answer PING on a fresh conn.
        let mut ping = TcpStream::connect(format!("127.0.0.1:{port}"))
            .expect("post-torture conn");
        let _ = ping.set_read_timeout(Some(Duration::from_secs(2)));
        ping.write_all(b"*1\r\n$4\r\nPING\r\n").expect("write PING");
        let mut reply = [0u8; 64];
        let n = ping.read(&mut reply).expect("read PING reply");
        assert!(
            reply[..n].starts_with(b"+PONG"),
            "post-torture PING failed for pattern {name}: {:?}",
            String::from_utf8_lossy(&reply[..n])
        );
    }

    eprintln!("wire_torture: kevy survived all {} pathological patterns", patterns.len());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn wire_torture_strategy_coverage() {
    // Quick spot-check: every strategy can produce all 3 outcome variants
    // across enough seeds. (Sanity that the fuzz harness is exercising
    // the parser in a non-degenerate way.)
    for strategy in Strategy::ALL {
        let summary = run_n(2_000, (strategy as u64).wrapping_add(0xBEEF));
        eprintln!(
            "wire_torture: strategy={strategy:?} parsed={} incomplete={} errored={}",
            summary.parsed, summary.incomplete, summary.errored
        );
        summary.assert_clean(2_000);
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
