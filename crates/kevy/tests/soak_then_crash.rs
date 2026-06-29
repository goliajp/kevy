//! Sustained-load soak chaos test — v1.34 step 4 of 5.
//!
//! Drives concurrent writes for `SOAK_SECONDS` (default 30, opt-in
//! `SOAK_SECONDS=3600` for real industrial-grade 1 h validation),
//! samples throughput every 5 s, then abruptly SIGKILLs and verifies
//! NO CORRUPTION on restart. Surfaces:
//!
//! - **Throughput drift** — if the per-window SET/s slows down over
//!   time (memory leak, AOF bloat, lost wakeup, etc.) the
//!   degradation-factor metric catches it.
//! - **Stuck workers** — if any 5 s window records ZERO ACKs (every
//!   writer thread got stuck), the strict per-window assert fires.
//! - **Persistence under sustained load** — same restart+verify pattern
//!   as the other chaos tests, but after a much longer write history.
//!
//! Strict asserts:
//! - NO CORRUPTION on restart.
//! - Every 5 s window had ≥ 1000 ACKs (else a writer thread stalled).
//!
//! Observational:
//! - Throughput-degradation factor = max_window_acks / min_window_acks.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test soak_then_crash --release -- --ignored --nocapture
//! # For 1 h soak:
//! SOAK_SECONDS=3600 cargo test -p kevy --test soak_then_crash \
//!     --release -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kevy_chaos::{
    AckEntry, Harness, HarnessConfig, KillSignal, WriterPool, pick_free_port,
    pipelined_verify_counts,
};

#[test]
#[ignore = "soak chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first; SOAK_SECONDS env var (default 30, set 3600 for 1 h)"]
fn soak_then_crash_no_corruption_no_degradation() {
    let soak_seconds: u64 = std::env::var("SOAK_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-soak-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    // 60 s spawn_timeout — the post-soak AOF can be 100s of MB and the
    // restart replay (single-threaded RESP parse + apply) takes
    // proportionally long. The default 10 s timeout was designed for
    // fresh-start scenarios.
    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.spawn_timeout = Duration::from_secs(60);
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(port, 4, Arc::clone(&stop));

    // Sample throughput every 5 s window.
    let mut window_acks: Vec<usize> = Vec::new();
    let start = Instant::now();
    let mut last_count = 0usize;
    let mut next_sample = start + Duration::from_secs(5);
    while start.elapsed() < Duration::from_secs(soak_seconds) {
        std::thread::sleep(Duration::from_millis(100));
        if Instant::now() >= next_sample {
            let now_count = pool.log.lock().unwrap().len();
            let window = now_count - last_count;
            window_acks.push(window);
            eprintln!(
                "soak: t={:>4}s window_acks={window} total={now_count}",
                start.elapsed().as_secs()
            );
            last_count = now_count;
            next_sample += Duration::from_secs(5);
        }
    }
    // Final partial window NOT pushed onto window_acks — it'd skew the
    // min/max because the loop exits as soon as elapsed > SOAK_SECONDS
    // and the partial window is sub-second.
    let now_count = pool.log.lock().unwrap().len();
    assert!(
        !window_acks.is_empty(),
        "vacuous test: 0 windows sampled (SOAK_SECONDS={soak_seconds} too short?)"
    );

    let pre_kill_acks = now_count;
    eprintln!("soak: {pre_kill_acks} total ACKs before SIGKILL");

    h.kill(KillSignal::Sigkill).expect("kill");
    stop.store(true, Ordering::Relaxed);
    let log = pool.join();
    let acks: Vec<AckEntry> = log.lock().unwrap().clone();
    eprintln!("soak: {} total ACKs (incl. post-kill drain)", acks.len());

    h.restart().expect("restart");

    // Strict: every 5 s window had ≥ 1000 ACKs.
    let min_window = *window_acks.iter().min().expect("at least one window");
    let max_window = *window_acks.iter().max().expect("at least one window");
    eprintln!(
        "soak: min_window={min_window} max_window={max_window} degradation_factor={:.2}",
        max_window as f64 / min_window.max(1) as f64
    );
    assert!(
        min_window >= 1000,
        "STALL DETECTED — a 5 s window had only {min_window} ACKs (expected ≥ 1000). \
         Window history: {window_acks:?}"
    );

    // Strict: NO CORRUPTION on restart.
    let (present, lost, corrupted) = pipelined_verify_counts(port, &acks);
    eprintln!(
        "soak: present={present}, lost={lost}, corrupted={}",
        corrupted.len()
    );
    assert!(
        corrupted.is_empty(),
        "CORRUPTION DETECTED — {} keys returned wrong values:\n{}",
        corrupted.len(),
        corrupted.join("\n")
    );

    let loss_fraction = lost as f64 / (acks.len() as f64).max(1.0);
    eprintln!(
        "soak: loss_fraction={:.2} % ({lost}/{}); strict no-corruption + no-stall asserts passed",
        loss_fraction * 100.0,
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
