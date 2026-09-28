//! A store whose process is killed keeps every write that returned, and
//! the next open — whatever it is configured to do — gets them back.
//! `mem::forget` stands in for the kill: no close runs, and with the reaper
//! in manual mode no background thread drains anything in the meantime.

#![cfg(feature = "persist")]

use kevy_embedded::{AppendFsync, Config, Store};

fn config(dir: &std::path::Path) -> Config {
    Config::default()
        .with_persist(dir)
        .with_appendfsync(AppendFsync::EverySec)
        .with_ttl_reaper_manual()
}

fn write_and_kill(cfg: Config, keys: std::ops::Range<u32>) {
    let dir = cfg.data_dir.clone().expect("a persistent store");
    let s = Store::open(cfg).expect("open");
    for i in keys {
        s.set(format!("k{i}").as_bytes(), format!("v{i}").as_bytes()).expect("set");
    }
    std::mem::forget(s);
    // a dead process's lock goes with it; here the forgotten store still
    // holds it, on a file the next open no longer finds
    std::fs::remove_file(dir.join("LOCK")).expect("the directory lock file");
}

fn assert_all(s: &Store, keys: std::ops::Range<u32>) {
    for i in keys {
        let got = s.get(format!("k{i}").as_bytes()).expect("get");
        assert_eq!(got.as_deref(), Some(format!("v{i}").as_bytes()), "k{i}");
    }
}

#[test]
fn staged_writes_come_back_after_a_kill() {
    let dir = kevy_tmpdir::TmpDir::new("killed-ring");
    write_and_kill(config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false), 0..300);
    let s = Store::open(config(dir.path()).with_mapped_aof(false)).expect("reopen");
    assert!(s.open_report().stage_recovered > 0, "the ring owed the log these writes");
    assert_all(&s, 0..300);
}

#[test]
fn mapped_writes_come_back_after_a_kill() {
    let dir = kevy_tmpdir::TmpDir::new("killed-mapped");
    write_and_kill(config(dir.path()).with_mapped_aof(true), 0..300);
    let s = Store::open(config(dir.path()).with_mapped_aof(true)).expect("reopen");
    assert_all(&s, 0..300);
    drop(s);
    let s = Store::open(config(dir.path()).with_mapped_aof(false)).expect("reopen through write()");
    assert_all(&s, 0..300);
}

#[test]
fn a_store_that_stops_staging_still_collects_what_its_ring_owed() {
    let dir = kevy_tmpdir::TmpDir::new("killed-unstaged");
    write_and_kill(config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false), 0..200);
    let s =
        Store::open(config(dir.path()).with_stage_ring(0).with_mapped_aof(false)).expect("reopen");
    assert!(s.open_report().stage_recovered > 0);
    assert_all(&s, 0..200);
    assert!(
        !dir.path().join("aof-0.aof.stage").exists(),
        "a store that does not stage keeps no ring"
    );
}

#[test]
fn a_reshard_after_a_kill_carries_what_the_ring_owed() {
    let dir = kevy_tmpdir::TmpDir::new("killed-reshard");
    write_and_kill(config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false), 0..400);
    let s = Store::open(config(dir.path()).with_shards(4).with_mapped_aof(false)).expect("reshard");
    assert_all(&s, 0..400);
    drop(s);
    let s = Store::open(config(dir.path()).with_shards(4).with_mapped_aof(false)).expect("reopen");
    assert_all(&s, 0..400);
}

#[test]
#[should_panic(expected = "a staging ring is 0 or a power of two of at least 64 KiB")]
fn a_ring_size_that_cannot_work_is_refused() {
    let _ = Config::default().with_stage_ring(100_000);
}
