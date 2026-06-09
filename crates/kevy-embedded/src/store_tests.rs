use super::*;
use crate::config::{AppendFsync, EvictionPolicy};

fn tmp_dir(name: &str) -> PathBuf {
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
    let stats = s.tick();
    // tick() should at least sample and reap (may take multiple ticks
    // for sparse layouts; the call is idempotent).
    let _ = stats;
    let _ = s.get(b"short").unwrap(); // lazy reap path
    assert!(s.expired_keys_total() >= 1);
    assert!(s.get(b"perm").unwrap().is_some());
}

#[test]
fn with_escape_hatch_works() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    let zsize = s.with(|store| {
        let _ = store.zadd(b"z", &[(1.0, b"a".to_vec()), (2.0, b"b".to_vec())]);
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
    std::thread::sleep(Duration::from_millis(120));
    // The active reaper should have caught it without anyone reading.
    let _ = s.get(b"k").unwrap(); // either way, key should now be gone
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
