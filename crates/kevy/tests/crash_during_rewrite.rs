//! Crash safety test — abrupt SIGKILL during a concurrent AOF rewrite.
//!
//! v1.33 chaos test. Forces frequent AOF rewrites by setting
//! `auto_aof_rewrite_min_size` very low, drives concurrent writes that
//! cross the threshold multiple times, then SIGKILLs at a random
//! point. Restart MUST recover every ACK'd write — the rewrite swap
//! is the most race-prone code path in Redis-family AOF persistence.
//!
//! Strict asserts:
//! - NO CORRUPTION (every present read matches its ACK'd value).
//! - At least one rewrite cycle completed before SIGKILL (`rewrites_total
//!   ≥ 1` in INFO after restart) — else the test didn't actually
//!   exercise the rewrite path and is vacuous.
//!
//! Observational metric:
//! - Loss-fraction (same shape as `crash_everysec`).
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test crash_during_rewrite --release -- --ignored --nocapture
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
fn crash_during_aof_rewrite_no_corruption() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-rewrite-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let cfg = HarnessConfig {
        kevy_bin: bin_path,
        // Force aggressive rewrites: every 256 KiB of AOF growth past
        // a 256 KiB floor triggers a rewrite. At 4 writers × ~100 k
        // SET/s × 40 B/op = 16 MB/s, this kicks off roughly 60+
        // rewrites in a 5 s pre-kill window — plenty to catch the
        // race.
        aof_rewrite_min_size: Some(256 * 1024),
        // 1 = 1 % growth past last-rewrite size triggers another rewrite.
        // (`auto_aof_rewrite_percentage = 0` would DISABLE — the kevy
        // convention matches Redis. We want frequent rewrites, hence 1.)
        aof_rewrite_pct: Some(1),
        ..HarnessConfig::new(tmp.clone(), port).with_fsync("everysec")
    };

    let mut h = Harness::spawn(cfg).expect("spawn kevy");
    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(port, 4, Arc::clone(&stop));
    // 5 s pre-kill window: at the configured aggression, kevy should
    // complete dozens of rewrite cycles in this time.
    std::thread::sleep(Duration::from_secs(5));
    let pre_kill_acks = pool.log.lock().unwrap().len();
    assert!(
        pre_kill_acks >= 1000,
        "vacuous test: only {pre_kill_acks} ACKs before kill"
    );
    // Snapshot the live kevy's `aof_rewrites_total` BEFORE killing.
    // The counter is in-memory and resets to 0 on restart, so the
    // "vacuous test" check must run pre-kill.
    let pre_kill_rewrites = info_aof_rewrites(port);
    eprintln!(
        "crash_during_rewrite: {pre_kill_acks} ACKs, {pre_kill_rewrites} \
         AOF rewrites before SIGKILL"
    );

    h.kill(KillSignal::Sigkill).expect("kill");
    stop.store(true, Ordering::Relaxed);
    let log = pool.join();
    let acks: Vec<AckEntry> = log.lock().unwrap().clone();
    eprintln!("crash_during_rewrite: {} total ACKs", acks.len());

    // Dump AOF sizes pre-restart (also catches the
    // ".rewrite" temp file if the rewrite was mid-flight at SIGKILL).
    for entry in std::fs::read_dir(&tmp).unwrap().flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n.starts_with("aof-") || n.ends_with(".rewrite") {
            let sz = std::fs::metadata(entry.path()).map(|m| m.len()).unwrap_or(0);
            eprintln!("  {n} = {sz} bytes");
        }
    }

    h.restart().expect("restart");

    // Dump kevy stderr (replay summary + any error).
    if let Ok(s) = std::fs::read_to_string(tmp.join("kevy.stderr.log")) {
        eprintln!("--- kevy.stderr.log (post-restart):");
        for line in s.lines().take(40) {
            eprintln!("  {line}");
        }
    }

    // Strict assert: at least one rewrite completed pre-kill (else the
    // test didn't exercise the rewrite path and is vacuous). The
    // counter resets across restart, so we use the pre-kill snapshot.
    assert!(
        pre_kill_rewrites >= 1,
        "vacuous test: 0 rewrites completed pre-kill — tune \
         aof_rewrite_min_size lower or pre-kill window longer"
    );

    let (present, lost, corrupted) = pipelined_verify_counts(port, &acks);
    eprintln!(
        "crash_during_rewrite: present={present}, lost={lost}, corrupted={}",
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
        "crash_during_rewrite: loss_fraction={:.2} % ({lost}/{}); strict no-corruption assert passed",
        loss_fraction * 100.0,
        acks.len()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Issue `INFO persistence` and parse `aof_rewrite_total:N` (sum
/// across shards if multiple replies). Returns 0 on any error so the
/// test surfaces a missing rewrite via the strict ≥1 assert rather
/// than a misleading panic.
fn info_aof_rewrites(port: u16) -> u64 {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(format!("127.0.0.1:{port}")) else { return 0 };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.write_all(b"*2\r\n$4\r\nINFO\r\n$11\r\npersistence\r\n");
    let mut buf = vec![0u8; 16 * 1024];
    let n = s.read(&mut buf).unwrap_or(0);
    let body = String::from_utf8_lossy(&buf[..n]);
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("aof_rewrites_total:") {
            if let Ok(v) = rest.trim().parse::<u64>() {
                return v;
            }
        }
    }
    0
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
