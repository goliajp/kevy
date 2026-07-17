use super::*;
use crate::KevyMetric;
use crate::config::{AppendFsync, EvictionPolicy};
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn tmp_dir(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("kevy-embedded-{name}-{uniq}"));
    p
}

#[test]
fn in_memory_roundtrip() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    s.set(b"k", b"v").unwrap();
    assert_eq!(s.get(b"k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s.dbsize(), 1);
    s.del(&[b"k"]).unwrap();
    assert_eq!(s.dbsize(), 0);
}

#[test]
fn persistence_round_trip_via_aof() {
    let dir = tmp_dir("aof-rt");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always),
        )
        .unwrap();
        for i in 0..50 {
            s.set(format!("k{i}").as_bytes(), b"v").unwrap();
        }
        s.incr_by(b"counter", 41).unwrap();
        s.hset(b"h", &[(b"field" as &[u8], b"val" as &[u8])]).unwrap();
    }
    // Reopen: AOF replay should reconstruct exactly the same state.
    let s2 = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s2.dbsize(), 52); // 50 + counter + h
    assert_eq!(s2.get(b"k0").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s2.get(b"k49").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s2.get(b"counter").unwrap(), Some(b"41".to_vec()));
    assert_eq!(s2.hget(b"h", b"field").unwrap(), Some(b"val".to_vec()));
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn eviction_works_under_pressure() {
    let s = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_max_memory(800)
            .with_eviction(EvictionPolicy::AllKeysLru),
    )
    .unwrap();
    for i in 0..50 {
        s.set(format!("k{i:02}").as_bytes(), b"xxxxxxxxxxxxxxxxxxxx")
            .unwrap();
    }
    assert!(s.used_memory() <= 800, "got {}", s.used_memory());
    assert!(s.evictions_total() > 0);
}

#[test]
fn manual_tick_runs_active_reaper() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    s.set_with_ttl(b"short", b"v", Duration::from_millis(1)).unwrap();
    s.set(b"perm", b"v").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    // Read path is non-mutating now: an expired key reads as None but is NOT
    // reclaimed by the get itself — the active reaper does that.
    assert_eq!(s.get(b"short").unwrap(), None);
    assert!(s.get(b"perm").unwrap().is_some());
    // tick() runs the active reaper; sampling is probabilistic, so tick until
    // it has reclaimed the expired key (bounded — one key is caught quickly).
    for _ in 0..50 {
        if s.expired_keys_total() >= 1 {
            break;
        }
        s.tick();
    }
    assert!(s.expired_keys_total() >= 1, "active reaper should reclaim the expired key");
    assert!(s.get(b"perm").unwrap().is_some());
}

#[test]
fn with_escape_hatch_works() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    let zsize = s.with(|store| {
        let _ = store.zadd(b"z", &[(1.0, b"a".as_slice()), (2.0, b"b".as_slice())]);
        store.zcard(b"z").unwrap()
    });
    assert_eq!(zsize, 2);
    // Direct (un-logged) write through `with`: caller may explicitly
    // log if they want it crash-safe. Here we just verify it landed.
    assert_eq!(s.type_of(b"z"), "zset");
}

#[test]
fn background_reaper_thread_drops_expired_keys() {
    let s = Store::open(
        Config::default().with_reaper_interval(Duration::from_millis(20)),
    )
    .unwrap();
    s.set_with_ttl(b"k", b"v", Duration::from_millis(5)).unwrap();
    // The active reaper (20ms interval) reclaims the expired key on its own —
    // reads no longer reap. Poll for it (bounded) rather than racing a fixed
    // sleep against the reaper thread's scheduling on a loaded CI box.
    for _ in 0..200 {
        if s.dbsize() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(s.get(b"k").unwrap(), None); // expired → gone
    assert_eq!(s.dbsize(), 0);
}

#[test]
fn arc_sharing_across_threads() {
    use std::sync::Arc;
    let s = Arc::new(Store::open(Config::default().with_ttl_reaper_manual()).unwrap());
    let mut handles = Vec::new();
    for i in 0..8 {
        let s = Arc::clone(&s);
        handles.push(std::thread::spawn(move || {
            for j in 0..50 {
                s.set(format!("t{i}-{j}").as_bytes(), b"v").unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(s.dbsize(), 8 * 50);
}

#[test]
fn drop_during_reaper_does_not_deadlock() {
    // Sanity: a Store with a Background reaper must drop cleanly even
    // while the reaper is sleeping. Without the stop-flag + join the
    // drop would either hang or race the reaper holding the mutex.
    for _ in 0..4 {
        let s = Store::open(
            Config::default().with_reaper_interval(Duration::from_millis(5)),
        )
        .unwrap();
        s.set(b"k", b"v").unwrap();
        // Let the reaper actually run a couple of times.
        std::thread::sleep(Duration::from_millis(40));
        drop(s); // must return within a few ms
    }
}

#[test]
fn save_snapshot_then_restart() {
    let dir = tmp_dir("snap-rt");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .without_aof()
                .with_ttl_reaper_manual(),
        )
        .unwrap();
        for i in 0..10 {
            s.set(format!("k{i}").as_bytes(), b"v").unwrap();
        }
        let saved = s.save_snapshot().unwrap();
        assert!(saved);
    }
    let s2 = Store::open(
        Config::default()
            .with_persist(&dir)
            .without_aof()
            .with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s2.dbsize(), 10);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn info_and_introspection() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    s.set(b"a", b"1").unwrap();
    s.set_with_ttl(b"b", b"2", Duration::from_secs(100)).unwrap();

    let info = s.info();
    assert_eq!(info.keys, 2);
    assert_eq!(info.expire_pending, 1);
    assert_eq!(info.aof_bytes, 0); // pure in-memory
    assert_eq!(s.expire_pending_count(), 1);

    assert!(s.ttl(b"b").unwrap() > Duration::from_secs(90));
    assert_eq!(s.ttl(b"a"), None); // key exists, no TTL
    assert_eq!(s.ttl(b"missing"), None); // no key
}

#[test]
fn metric_sink_receives_rewrite_and_replay() {
    use std::sync::{Arc, Mutex};
    let dir = tmp_dir("metric-sink");
    let seen: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    // Run 1: redundant writes + a manual rewrite → a Rewrite event fires.
    {
        let s_seen = seen.clone();
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always)
                .with_metric_sink(move |m| {
                    let tag = match m {
                        KevyMetric::Rewrite { .. } => "rewrite",
                        KevyMetric::Replay { .. } => "replay",
                    };
                    s_seen.lock().unwrap().push(tag);
                }),
        )
        .unwrap();
        for i in 0..50 {
            s.set(b"k", format!("v{i}").as_bytes()).unwrap();
        }
        assert!(s.rewrite_aof().unwrap().is_some());
    }
    assert!(
        seen.lock().unwrap().contains(&"rewrite"),
        "manual rewrite_aof should emit a Rewrite metric"
    );

    // Run 2: reopen → AOF replay → a Replay event fires.
    seen.lock().unwrap().clear();
    let r_seen = seen.clone();
    let _s2 = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_ttl_reaper_manual()
            .with_metric_sink(move |m| {
                if matches!(m, KevyMetric::Replay { .. }) {
                    r_seen.lock().unwrap().push("replay");
                }
            }),
    )
    .unwrap();
    assert!(
        seen.lock().unwrap().contains(&"replay"),
        "reopening should emit a Replay metric"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn auto_aof_rewrite_compacts_redundant_writes() {
    let dir = tmp_dir("auto-rw");
    let s = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_ttl_reaper_manual()
            .with_appendfsync(AppendFsync::Always)
            // Aggressive thresholds so a couple hundred SETs trip it.
            .with_auto_aof_rewrite(1, 1),
    )
    .unwrap();
    // Same key overwritten many times → the AOF holds 300 redundant SETs.
    for i in 0..300 {
        s.set(b"hot", format!("v{i}").as_bytes()).unwrap();
    }
    let before = s.info().aof_bytes;
    s.tick(); // manual mode: tick drives the auto-rewrite check
    let after = s.info().aof_bytes;

    assert!(
        after < before,
        "auto-rewrite should compact redundant SETs: before={before} after={after}"
    );
    // Latest value preserved, and it survives a real restart.
    assert_eq!(s.get(b"hot").unwrap(), Some(b"v299".to_vec()));
    drop(s);
    let s2 = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s2.get(b"hot").unwrap(), Some(b"v299".to_vec()));
    assert_eq!(s2.dbsize(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `save_snapshot` must reset the AOF to post-collect writes (the
/// documented snapshot+log durability contract). Before the fix the AOF
/// kept its full history, so a restart replayed pre-snapshot commands ON
/// TOP of the snapshot — non-idempotent ops (RPUSH) duplicated.
#[test]
fn save_snapshot_resets_aof_no_double_replay() {
    let dir = tmp_dir("save-aof-reset");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        s.rpush(b"l", &[b"a", b"b"]).unwrap();
        assert!(s.save_snapshot().unwrap());
        // Post-snapshot write: must survive via the (reset) AOF.
        s.rpush(b"l", &[b"c"]).unwrap();
    }
    let s2 = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(
        s2.llen(b"l").unwrap(),
        3,
        "snapshot + replayed AOF must not double-apply pre-snapshot RPUSHes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// open_report(): the machine-readable twin of the boot WARN line. A clean
// open reports zero drops; an open over a damaged AOF reports the dropped
// bytes, the corrupt flag, and the quarantine file the repair wrote — the
// signal a host turns into a startup health check (the 3-day silent-loss
// incident was this exact signal living only in stderr).
#[test]
fn open_report_surfaces_drops_corruption_and_quarantine() {
    let dir = tmp_dir("open-report");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always),
        )
        .unwrap();
        for i in 0..20 {
            s.set(format!("k{i}").as_bytes(), b"v").unwrap();
        }
        let clean = s.open_report();
        assert_eq!(clean.dropped_bytes, 0);
        assert!(!clean.corrupt);
        assert!(clean.quarantine_paths.is_empty());
    }
    // Damage the AOF at a frame boundary: a frame whose bulk header lies
    // about its payload length (the mailrs incident's exact parser error),
    // followed by a WELL-FORMED frame that the stop drops — random interior
    // byte-flips get absorbed by RESP's lenient inline-command form and
    // don't reliably produce a corrupt verdict.
    let aof = dir.join("aof-0.aof");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&aof).unwrap();
        f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nq\r\n$9\r\nab\r\n").unwrap();
        f.write_all(b"*3\r\n$3\r\nSET\r\n$4\r\ngood\r\n$4\r\ntail\r\n").unwrap();
    }
    let s = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    let r = s.open_report();
    assert!(r.dropped_bytes > 0, "damage must be reported: {r:?}");
    assert!(r.corrupt, "parser-failing damage must set corrupt: {r:?}");
    assert_eq!(r.quarantine_paths.len(), 1, "repair must quarantine: {r:?}");
    assert!(r.quarantine_paths[0].exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// shutdown(): the two-line signal-handler contract. After it returns, every
// pre-shutdown write is on disk (a real fsync, not the EverySec window) and
// every later write is refused with Closed — while reads stay available and
// any clone's shutdown gates all clones.
#[test]
fn shutdown_fsyncs_refuses_writes_and_survives_reopen() {
    let dir = tmp_dir("shutdown");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        let clone = s.clone();
        for i in 0..50 {
            s.set(format!("k{i}").as_bytes(), b"v").unwrap();
        }
        s.shutdown().unwrap();
        // Writes refused — on the clone too (the flag is shared).
        assert!(matches!(s.set(b"late", b"x"), Err(crate::KevyError::Closed)));
        assert!(matches!(clone.set(b"late", b"x"), Err(crate::KevyError::Closed)));
        assert!(matches!(s.incr_by(b"n", 1), Err(crate::KevyError::Closed)));
        // Reads stay available.
        assert_eq!(s.get(b"k0").unwrap(), Some(b"v".to_vec()));
        // Idempotent.
        s.shutdown().unwrap();
    }
    // Everything written before shutdown survived the (clean) teardown.
    let s = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s.get(b"k49").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s.get(b"late").unwrap(), None, "refused write must not exist");
    let _ = std::fs::remove_dir_all(&dir);
}
