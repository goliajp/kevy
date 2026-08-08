//! The short-lived-process AOF trap, from the smix feedback (2026-08-09).
//!
//! A CLI that opens the same store directory once per command hit 100 MB
//! of AOF for 3 live keys: `Aof::open` baselines the growth rule at the
//! current file size, each run appends a few KB and exits, so the +pct%
//! trigger never fires — the growth is cross-process, the baseline was
//! per-process. The fix anchors the baseline to the live image's
//! estimated rewrite size after replay, restoring the rule's meaning
//! ("the log is pct% history") across processes.

#![cfg(feature = "persist")]

use kevy_embedded::{Config, Store};

fn aof_size(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".aof"))
        .map(|e| e.metadata().map_or(0, |m| m.len()))
        .sum()
}

/// Reopen-write-exit in a loop (the CLI shape), then hold one open long
/// enough for the reaper's auto-rewrite to run. The log must compact —
/// before the baseline fix it could not: every open re-anchored the
/// baseline at the ever-larger file, so no run ever saw +100% growth.
#[test]
fn short_lived_reopens_do_not_grow_the_log_without_bound() {
    let dir = kevy_tmpdir::TmpDir::new("aof-shortlived");
    let config = || {
        Config::default()
            .with_persist(dir.path())
            // Small min_size so the test reaches the trigger zone in KBs.
            .with_auto_aof_rewrite(100, 4096)
    };
    let value = [b'v'; 256];
    // The history: many short-lived processes, one live key.
    for _ in 0..40 {
        let store = Store::open(config()).expect("open");
        for _ in 0..20 {
            store.set(b"the-one-key", &value).expect("set");
        }
        drop(store);
    }
    let grown = aof_size(dir.path());
    assert!(
        grown > 100 * 1024,
        "precondition: history should dominate ({grown} bytes)"
    );
    // One more open, held across reaper ticks: the anchored baseline sees
    // the file as ~100% history and the auto-rewrite compacts it.
    let store = Store::open(config()).expect("open");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut compacted = aof_size(dir.path());
    while std::time::Instant::now() < deadline {
        compacted = aof_size(dir.path());
        if compacted < grown / 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(store);
    assert!(
        compacted < grown / 2,
        "auto-rewrite never fired: {grown} bytes of history stayed at {compacted}"
    );
    // And the data survived the compaction.
    let store = Store::open(config()).expect("reopen");
    assert_eq!(
        store.get(b"the-one-key").expect("get").as_deref(),
        Some(&value[..])
    );
}
