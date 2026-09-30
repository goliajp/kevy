//! A restore applies each logged write exactly once, whatever order
//! snapshots and rewrites happened in and wherever a snapshot commit
//! stopped.

use crate::log_base::{LogHead, prev_path, read_log_head, snapshot_id};
use crate::tests_rewrite::apply_for_test;
use crate::{Aof, Argv, Fsync, load_snapshot, replay_aof, settle_snapshot, write_snapshot_tmp};
use kevy_store::Store;
use std::path::{Path, PathBuf};

fn rpush(store: &mut Store, aof: &mut Aof, item: &[u8]) {
    store.rpush(b"l", &[item]).unwrap();
    aof.append(&Argv::from(vec![b"RPUSH".to_vec(), b"l".to_vec(), item.to_vec()])).unwrap();
}

/// What a restart restores from `dir`, through the same rule every
/// runtime uses.
fn restore(snap: &Path, log: &Path) -> Vec<Vec<u8>> {
    let mut store = Store::new();
    if settle_snapshot(snap, Some(log)).unwrap() {
        load_snapshot(&mut store, snap).unwrap();
    }
    replay_aof(log, |args| apply_for_test(&mut store, &args)).unwrap();
    store.lrange(b"l", 0, -1).unwrap()
}

fn files(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = kevy_tmpdir::unique_dir(name);
    (dir.join("dump-0.rdb"), dir.join("aof-0.aof"), dir)
}

fn save(store: &Store, aof: &mut Aof, snap: &Path) {
    let reset = aof.begin_view_rewrite().unwrap();
    let tmp = write_snapshot_tmp(store, snap).unwrap();
    aof.commit_snapshot(&tmp, snap, &reset).unwrap();
}

fn items(s: &[&str]) -> Vec<Vec<u8>> {
    s.iter().map(|i| i.as_bytes().to_vec()).collect()
}

/// The reported sequence: a snapshot, then a rewrite. The rewritten log is
/// a complete image; the snapshot under it must not be loaded.
#[test]
fn a_rewrite_after_a_snapshot_restores_each_write_once() {
    let (snap, log, dir) = files("pair-save-rewrite");
    let mut store = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    for i in ["a", "b", "c"] {
        rpush(&mut store, &mut aof, i.as_bytes());
    }
    save(&store, &mut aof, &snap);
    rpush(&mut store, &mut aof, b"d");
    aof.rewrite_from(&store).unwrap();
    rpush(&mut store, &mut aof, b"e");
    drop(aof);
    assert_eq!(read_log_head(&log).unwrap(), LogHead::StandsAlone);
    assert_eq!(restore(&snap, &log), items(&["a", "b", "c", "d", "e"]));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The other order: the snapshot after the rewrite continues it, and the
/// log restarts from the snapshot.
#[test]
fn a_snapshot_after_a_rewrite_restores_each_write_once() {
    let (snap, log, dir) = files("pair-rewrite-save");
    let mut store = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    for i in ["a", "b"] {
        rpush(&mut store, &mut aof, i.as_bytes());
    }
    aof.rewrite_from(&store).unwrap();
    rpush(&mut store, &mut aof, b"c");
    save(&store, &mut aof, &snap);
    rpush(&mut store, &mut aof, b"d");
    drop(aof);
    let id = snapshot_id(&snap).unwrap().expect("a committed snapshot carries its id");
    assert_eq!(read_log_head(&log).unwrap(), LogHead::After(id));
    assert_eq!(restore(&snap, &log), items(&["a", "b", "c", "d"]));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A crash after the new snapshot's rename and before the log reset: the
/// old log continues the previous snapshot, which was kept aside.
#[test]
fn a_crash_before_the_log_reset_restores_the_previous_pair() {
    let (snap, log, dir) = files("pair-crash-mid");
    let mut store = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    rpush(&mut store, &mut aof, b"a");
    save(&store, &mut aof, &snap);
    rpush(&mut store, &mut aof, b"b");
    aof.sync_now().unwrap();
    // the second commit's first two steps, then the process dies
    let tmp = write_snapshot_tmp(&store, &snap).unwrap();
    std::fs::rename(&snap, prev_path(&snap)).unwrap();
    std::fs::rename(&tmp, &snap).unwrap();
    drop(aof);
    assert_eq!(restore(&snap, &log), items(&["a", "b"]));
    assert!(!prev_path(&snap).exists(), "the kept snapshot is back in place");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A crash after the log reset and before the kept snapshot is removed:
/// the commit is complete, the kept snapshot is dropped.
#[test]
fn a_crash_after_the_log_reset_keeps_the_new_pair() {
    let (snap, log, dir) = files("pair-crash-late");
    let mut store = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    rpush(&mut store, &mut aof, b"a");
    save(&store, &mut aof, &snap);
    let stale = std::fs::read(&snap).unwrap();
    rpush(&mut store, &mut aof, b"b");
    save(&store, &mut aof, &snap);
    rpush(&mut store, &mut aof, b"c");
    drop(aof);
    std::fs::write(prev_path(&snap), stale).unwrap();
    assert_eq!(restore(&snap, &log), items(&["a", "b", "c"]));
    assert!(!prev_path(&snap).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A log that continues a snapshot that is gone or replaced is refused,
/// not restored alone or over the wrong snapshot.
#[test]
fn a_log_whose_snapshot_is_not_there_is_refused() {
    let (snap, log, dir) = files("pair-missing");
    let mut store = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    rpush(&mut store, &mut aof, b"a");
    save(&store, &mut aof, &snap);
    drop(aof);
    let other = dir.join("other.rdb");
    crate::save_snapshot(&store, &other).unwrap();
    std::fs::rename(&other, &snap).unwrap();
    let err = settle_snapshot(&snap, Some(&log)).unwrap_err();
    assert!(err.to_string().contains("is snapshot"), "{err}");
    std::fs::remove_file(&snap).unwrap();
    let err = settle_snapshot(&snap, Some(&log)).unwrap_err();
    assert!(err.to_string().contains("is missing"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A log from before logs named their snapshot keeps the old rule, and a
/// shard without a log restores its snapshot.
#[test]
fn a_log_without_a_base_restores_over_the_snapshot() {
    let (snap, log, dir) = files("pair-legacy");
    let mut store = Store::new();
    store.rpush(b"l", &[b"a"]).unwrap();
    crate::save_snapshot(&store, &snap).unwrap();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    rpush(&mut store, &mut aof, b"b");
    drop(aof);
    assert_eq!(read_log_head(&log).unwrap(), LogHead::Legacy);
    assert_eq!(restore(&snap, &log), items(&["a", "b"]));
    assert!(settle_snapshot(&snap, None).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The base frame is not a command: a replay neither applies nor counts it.
#[test]
fn the_base_frame_is_not_replayed() {
    let (_, log, dir) = files("pair-replay");
    let mut store = Store::new();
    store.rpush(b"l", &[b"a"]).unwrap();
    Aof::open(&log, Fsync::No).unwrap().rewrite_from(&store).unwrap();
    let mut seen = Vec::new();
    let report = replay_aof(&log, |args| seen.push(args[0].to_vec())).unwrap();
    assert_eq!(seen, [b"RPUSH".to_vec()]);
    assert_eq!(report.commands, 1);
    std::fs::remove_dir_all(&dir).unwrap();
}
