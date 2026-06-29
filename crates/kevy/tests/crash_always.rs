//! Crash safety test — `appendfsync = always` zero-loss contract.
//!
//! Spawn kevy with the strictest fsync policy, drive concurrent writes,
//! SIGKILL mid-flight, restart, verify every ACK'd write is present.
//!
//! Gated `#[ignore]` so default `cargo test` skips it. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test crash_always --release -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, KillSignal, WriterPool, pick_free_port, verify_all_present};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn crash_always_fsync_zero_loss() {
    let bin = std::env::var("KEVY_BIN").ok().map(PathBuf::from);
    let bin_path = bin.unwrap_or_else(|| {
        // Walk up to workspace root for target/release/kevy.
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
    });

    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-always-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let cfg = HarnessConfig {
        kevy_bin: bin_path,
        port,
        threads: 2,
        data_dir: tmp.clone(),
        appendfsync: "always".to_string(),
        spawn_timeout: Duration::from_secs(10),
    };

    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    // Phase 1: concurrent writers for T seconds.
    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(port, 4, Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(2));

    // Snapshot ACK count BEFORE SIGKILL so we know a meaningful number
    // of writes accumulated (else the test is vacuous).
    let pre_kill_acks = pool.log.lock().unwrap().len();
    assert!(
        pre_kill_acks >= 100,
        "vacuous test: only {pre_kill_acks} ACKs before kill — slow CI or kevy broken?"
    );
    eprintln!("crash_always: {pre_kill_acks} ACKs before SIGKILL");

    // Phase 2: abrupt SIGKILL.
    h.kill(KillSignal::Sigkill).expect("kill");
    stop.store(true, Ordering::Relaxed);
    let log = pool.join();
    let acks = log.lock().unwrap().clone();
    eprintln!("crash_always: {} total ACKs (some after the kill snapshot)", acks.len());

    // Phase 3: restart on the same data dir.
    h.restart().expect("restart");

    // Phase 4: every ACK'd write must be present (zero-loss for always-fsync).
    verify_all_present(port, &acks).expect("zero-loss verification");

    // Cleanup happens via Harness::Drop.
    let _ = std::fs::remove_dir_all(&tmp);
}
