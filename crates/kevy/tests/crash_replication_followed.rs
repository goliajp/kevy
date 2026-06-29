//! Crash safety test — primary dies under load; verify replica caught
//! up close to the ACK'd write set.
//!
//! v1.33 chaos test. Spawns a primary kevy + a replica kevy, drives
//! concurrent writes against the primary, abruptly SIGKILLs the
//! primary mid-flight, then queries the REPLICA and counts how many
//! of the primary-ACK'd writes made it. The lost-fraction here is
//! the replication lag at kill time — writes that the primary
//! acknowledged but hadn't streamed-and-applied on the replica yet.
//!
//! Strict asserts:
//! - NO CORRUPTION on the replica (every present read matches its
//!   primary-ACK'd value).
//! - Replica is reachable post-kill (the test must successfully
//!   query the replica or assertion fires).
//!
//! Observational metric:
//! - Replication-lag lost-fraction (replica-side present / total ACKs).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test crash_replication_followed --release -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_chaos::{
    AckEntry, Harness, HarnessConfig, KillSignal, WriterPool, pick_free_port,
    pipelined_verify_counts,
};

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn crash_replication_followed_no_corruption() {
    let bin_path = resolve_kevy_bin();
    let primary_port = pick_free_port().expect("primary port");
    let replica_port = pick_free_port().expect("replica port");
    let primary_replication_base = pick_free_port().expect("primary repl port");
    let primary_tmp =
        std::env::temp_dir().join(format!("kevy-chaos-primary-{primary_port}"));
    let replica_tmp =
        std::env::temp_dir().join(format!("kevy-chaos-replica-{replica_port}"));
    let _ = std::fs::remove_dir_all(&primary_tmp);
    let _ = std::fs::remove_dir_all(&replica_tmp);

    let primary_cfg = HarnessConfig {
        kevy_bin: bin_path.clone(),
        threads: 1, // single shard — replication topology is simpler.
        ..HarnessConfig::new(primary_tmp.clone(), primary_port)
            .with_fsync("everysec")
            .with_extra_toml(format!(
                "[replication]\nrole = \"primary\"\nlisten_port_base = {primary_replication_base}\n"
            ))
    };
    let mut primary = Harness::spawn(primary_cfg).expect("spawn primary");

    // Give the primary a moment to bring up its replication listener
    // before the replica tries to connect.
    std::thread::sleep(Duration::from_millis(200));

    let replica_cfg = HarnessConfig {
        kevy_bin: bin_path,
        threads: 1, // must match the primary's shard count.
        ..HarnessConfig::new(replica_tmp.clone(), replica_port)
            .with_fsync("everysec")
            .with_extra_toml(format!(
                // `upstream` points at the primary's REPLICATION listener
                // base, not its CLIENT port. Shard-aware replica client
                // connects to {base, base+1, ..., base+nshards-1}.
                "[replication]\nrole = \"replica\"\nupstream = \"127.0.0.1:{primary_replication_base}\"\n"
            ))
    };
    let replica = Harness::spawn(replica_cfg).expect("spawn replica");

    // Let replication handshake complete + a brief warm period.
    std::thread::sleep(Duration::from_millis(500));

    // Phase 1: drive concurrent writes against the PRIMARY only.
    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(primary_port, 4, Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(3));
    let pre_kill_acks = pool.log.lock().unwrap().len();
    assert!(
        pre_kill_acks >= 1000,
        "vacuous test: only {pre_kill_acks} primary-ACKs before kill"
    );
    eprintln!("crash_replication: {pre_kill_acks} primary-ACKs before SIGKILL");

    // Phase 2: SIGKILL the primary mid-flight.
    primary.kill(KillSignal::Sigkill).expect("kill primary");
    stop.store(true, Ordering::Relaxed);
    let log = pool.join();
    let acks: Vec<AckEntry> = log.lock().unwrap().clone();
    eprintln!("crash_replication: {} total primary-ACKs", acks.len());

    // Give the replica a chance to drain in-flight frames + commit them
    // (the primary is dead so no new frames will arrive; this just
    // accounts for the network/dispatch latency of frames already
    // sent before SIGKILL).
    std::thread::sleep(Duration::from_secs(2));

    // Phase 3: verify against the REPLICA. The replica may still have
    // some commands pending in its inbox; the lost-fraction we measure
    // reflects the replication lag at kill time.
    let (present, lost, corrupted) = pipelined_verify_counts(replica_port, &acks);
    eprintln!(
        "crash_replication: replica present={present}, lost={lost}, corrupted={}",
        corrupted.len()
    );

    // Dump both kevy stderr logs for diagnostic visibility.
    if let Ok(s) = std::fs::read_to_string(primary_tmp.join("kevy.stderr.log")) {
        eprintln!("--- primary kevy.stderr.log (truncated):");
        for line in s.lines().take(20) {
            eprintln!("  {line}");
        }
    }
    if let Ok(s) = std::fs::read_to_string(replica_tmp.join("kevy.stderr.log")) {
        eprintln!("--- replica kevy.stderr.log (truncated):");
        for line in s.lines().take(20) {
            eprintln!("  {line}");
        }
    }

    assert!(
        corrupted.is_empty(),
        "CORRUPTION DETECTED on replica — {} keys returned wrong values:\n{}",
        corrupted.len(),
        corrupted.join("\n")
    );

    let replication_lag_fraction = lost as f64 / (acks.len() as f64).max(1.0);
    eprintln!(
        "crash_replication: replication_lag_fraction={:.2} % ({lost}/{}); strict no-corruption assert passed",
        replication_lag_fraction * 100.0,
        acks.len()
    );

    // Cleanup (replica still running; drop runs the kill).
    let _ = std::fs::remove_dir_all(&primary_tmp);
    let _ = std::fs::remove_dir_all(&replica_tmp);
    // Replica gets dropped here, killing it cleanly.
    drop(replica);
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
