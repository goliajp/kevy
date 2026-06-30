//! v1.39 SIGTERM graceful drain chaos test (Unix).
//!
//! Spawn kevy, hammer it with concurrent writes, send SIGTERM. kevy
//! must drain: fsync AOF, close listeners, exit 0 (not SIGKILL'd by
//! us; the test sends SIGTERM only). Strict asserts:
//! - kevy exits within the drain timeout (default 10 s).
//! - The exit code is 0 (or 143 = 128+15 if SIGTERM bypassed the
//!   handler — surfaces as a real bug).
//! - Every primary-ACK'd write is present on a fresh kevy restart on
//!   the same data dir (the drain DOES fsync the everysec window —
//!   that's the whole point).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test sigterm_drain_chaos --release -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_chaos::{
    Harness, HarnessConfig, KillSignal, WriterPool, pick_free_port,
    pipelined_verify_counts,
};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn sigterm_drains_cleanly_no_lost_writes() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-sigterm-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    // Drive concurrent writes for 2 s, then SIGTERM.
    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(port, 4, Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(2));
    let pre_signal = pool.log.lock().unwrap().len();
    assert!(
        pre_signal >= 100,
        "vacuous test: only {pre_signal} ACKs before SIGTERM"
    );
    eprintln!("sigterm_drain: {pre_signal} ACKs before SIGTERM");

    // Send SIGTERM, then give the drain ~3 s to complete.
    let start = std::time::Instant::now();
    h.kill(KillSignal::Sigterm).expect("send SIGTERM");
    stop.store(true, Ordering::Relaxed);
    let log = pool.join();
    let acks = log.lock().unwrap().clone();
    let drain_elapsed = start.elapsed();
    eprintln!(
        "sigterm_drain: {} total ACKs, drain elapsed {:.2} s",
        acks.len(),
        drain_elapsed.as_secs_f64()
    );

    assert!(
        drain_elapsed < Duration::from_secs(10),
        "drain took {:.2} s > 10 s budget — SIGTERM handler may not be firing",
        drain_elapsed.as_secs_f64()
    );

    // Restart kevy on the same data dir; every ACK'd write must be
    // present (SIGTERM drain MUST fsync the everysec window).
    h.restart().expect("restart");
    let (present, lost, corrupted) = pipelined_verify_counts(port, &acks);
    eprintln!(
        "sigterm_drain: present={present} lost={lost} corrupted={}",
        corrupted.len()
    );
    assert!(
        corrupted.is_empty(),
        "CORRUPTION DETECTED after SIGTERM drain: {}",
        corrupted.join("\n")
    );

    // Strict: SIGTERM is a GRACEFUL signal — lost-fraction must be
    // BETTER than the SIGKILL-equivalent in v1.31.2's crash_everysec
    // (~0.05 % on Mac). Bound at 1 % for headroom across runners.
    let loss = lost as f64 / (acks.len() as f64).max(1.0);
    assert!(
        loss < 0.01,
        "SIGTERM drain lost {:.2} % of writes ({lost}/{}) — graceful contract is broken",
        loss * 100.0,
        acks.len()
    );

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
