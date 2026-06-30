//! v1.40 backup / restore chaos test.
//!
//! Spawn kevy, drive concurrent writes, take a backup mid-stream
//! (after a BGSAVE to make the snapshot fresh), restore to a fresh
//! data_dir + start a NEW kevy on it, verify NO FABRICATION on every
//! ACK'd write that landed before the backup completed.
//!
//! Strict asserts:
//! - Backup round-trip preserves writes from before the backup
//!   moment (within everysec lost-window tolerance).
//! - NO CORRUPTION on the restored kevy.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test backup_restore_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_chaos::{
    AckEntry, Harness, HarnessConfig, KillSignal, WriterPool, pick_free_port,
    pipelined_verify_counts,
};
use kevy_cli::backup;

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn backup_restore_round_trip_no_fabrication() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-backup-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path.clone();
    cfg.threads = 2;
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    // Drive writes for 2 s.
    let stop = Arc::new(AtomicBool::new(false));
    let pool = WriterPool::spawn(port, 4, Arc::clone(&stop));
    std::thread::sleep(Duration::from_secs(2));
    let pre_backup_count = pool.log.lock().unwrap().len();
    assert!(
        pre_backup_count >= 1000,
        "vacuous test: only {pre_backup_count} ACKs before backup"
    );
    eprintln!("backup_restore: {pre_backup_count} ACKs before BGSAVE+backup");

    // Trigger BGSAVE + give it a moment.
    {
        let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).expect("BGSAVE conn");
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        s.write_all(b"*1\r\n$7\r\nBGSAVE\r\n").expect("write BGSAVE");
        let mut buf = [0u8; 64];
        let _ = s.read(&mut buf);
    }
    std::thread::sleep(Duration::from_millis(300));

    // Snapshot the ACK log AT the backup moment.
    let backup_acks: Vec<AckEntry> = pool.log.lock().unwrap().clone();
    eprintln!(
        "backup_restore: captured {} ACKs at backup moment",
        backup_acks.len()
    );

    // Run the in-process backup pack.
    let backup_path = tmp.join("dump.kevybkp");
    let bytes = backup::pack(&tmp, &backup_path).expect("pack backup");
    eprintln!("backup_restore: backup container = {bytes} bytes");

    // Let writers run a bit more, then stop and SIGKILL the live kevy
    // (we don't care about its post-backup state; the test is about
    // the backup's round-trip).
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let _ = pool.join();
    h.kill(KillSignal::Sigkill).expect("kill primary");

    // Restore into a fresh dir + start a NEW kevy against it.
    let restored_port = pick_free_port().expect("restored port");
    let restored_dir =
        std::env::temp_dir().join(format!("kevy-chaos-restored-{restored_port}"));
    let _ = std::fs::remove_dir_all(&restored_dir);
    backup::unpack(&backup_path, &restored_dir).expect("unpack");

    let mut cfg2 = HarnessConfig::new(restored_dir.clone(), restored_port)
        .with_fsync("everysec");
    cfg2.kevy_bin = bin_path;
    cfg2.threads = 2;
    // Restored AOF can be 80 MB+; replay takes several seconds. Bump
    // the spawn timeout generously.
    cfg2.spawn_timeout = Duration::from_secs(60);
    let _h2 = Harness::spawn(cfg2).expect("spawn restored kevy");

    // Verify: every ACK in `backup_acks` whose value still exists on
    // the restored kevy returns the ORIGINAL value (no fabrication).
    let (present, lost, corrupted) = pipelined_verify_counts(restored_port, &backup_acks);
    eprintln!(
        "backup_restore: restored kevy — present={present} lost={lost} corrupted={}",
        corrupted.len()
    );
    assert!(
        corrupted.is_empty(),
        "BACKUP/RESTORE CORRUPTION — {} keys returned wrong values:\n{}",
        corrupted.len(),
        corrupted.join("\n")
    );
    let recall_rate = present as f64 / backup_acks.len() as f64;
    eprintln!(
        "backup_restore: recall rate = {:.2} % ({present}/{}); strict no-fabrication passed",
        recall_rate * 100.0,
        backup_acks.len()
    );
    // Strict: recall MUST be substantial — at minimum 50 % (backup
    // taken DURING write storm; some writes may not have been in the
    // snapshot/AOF at the pack moment). Tune up if the test gets
    // tight on slow runners.
    assert!(
        recall_rate >= 0.5,
        "backup recall too low ({:.2} %) — either BGSAVE didn't flush or pack ran before AOF caught up",
        recall_rate * 100.0
    );

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&restored_dir);
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
