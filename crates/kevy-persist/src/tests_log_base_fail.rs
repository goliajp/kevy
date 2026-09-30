//! Where pairing a snapshot with its log meets a file system that says
//! no, and where a snapshot commit stops half way. Every condition is a
//! real one — a directory where a file should be, a path under a file, a
//! rename across two file systems, a directory left in each intermediate
//! state — and holds for any user, root included.

use crate::log_base::{
    LOG_BASE, LogHead, base_frame, base_of, prev_path, read_log_head, snapshot_id,
    write_snapshot_id,
};
use crate::tests_rewrite::apply_for_test;
use crate::{
    Aof, Argv, Fsync, load_snapshot, replay_aof, save_snapshot, settle_snapshot, write_snapshot_tmp,
};
use kevy_store::Store;
use std::path::{Path, PathBuf};

fn dir(name: &str) -> PathBuf {
    kevy_tmpdir::unique_dir(name)
}

fn store(items: &[&str]) -> Store {
    let mut s = Store::new();
    for i in items {
        s.rpush(b"l", &[i.as_bytes()]).unwrap();
    }
    s
}

fn push(store: &mut Store, aof: &mut Aof, item: &str) {
    store.rpush(b"l", &[item.as_bytes()]).unwrap();
    aof.append(&Argv::from(vec![b"RPUSH".to_vec(), b"l".to_vec(), item.as_bytes().to_vec()]))
        .unwrap();
}

/// What a restart restores, through the rule every runtime uses.
fn restore(snap: &Path, log: &Path) -> Vec<Vec<u8>> {
    let mut s = Store::new();
    if settle_snapshot(snap, Some(log)).unwrap() {
        load_snapshot(&mut s, snap).unwrap();
    }
    replay_aof(log, |args| apply_for_test(&mut s, &args)).unwrap();
    s.lrange(b"l", 0, -1).unwrap()
}

fn items(s: &[&str]) -> Vec<Vec<u8>> {
    s.iter().map(|i| i.as_bytes().to_vec()).collect()
}

/// A non-empty directory at `path`, which neither a rename nor an unlink
/// can replace or remove.
fn occupied(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::write(path.join("x"), b"x").unwrap();
}

/// A log written by hand: the magic, then these records.
fn log_of(path: &Path, frames: &[Argv]) {
    let mut bytes = crate::record::AOF2_MAGIC.to_vec();
    for f in frames {
        crate::record::write_record_multibulk(&mut bytes, f, &mut Vec::new()).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_snapshot_id_is_read_only_from_a_whole_footer() {
    let d = dir("fail-snapshot-id");
    std::fs::write(d.join("short"), b"KEVYSNID").unwrap();
    assert_eq!(snapshot_id(&d.join("short")).unwrap(), None, "shorter than a footer");
    std::fs::write(d.join("untagged"), [7u8; 40]).unwrap();
    assert_eq!(snapshot_id(&d.join("untagged")).unwrap(), None, "no tag");
    assert!(snapshot_id(&d.join("untagged").join("under-a-file")).is_err());
    occupied(&d.join("a-directory"));
    assert!(snapshot_id(&d.join("a-directory")).is_err());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_footer_that_cannot_be_written_is_an_error() {
    let d = dir("fail-footer");
    std::fs::write(d.join("f"), b"").unwrap();
    // a descriptor opened for reading refuses every write
    let mut read_only = std::fs::File::open(d.join("f")).unwrap();
    assert!(write_snapshot_id(&mut read_only).is_err());
    assert!(write_snapshot_tmp(&Store::new(), &d.join("no-such-dir").join("dump.rdb")).is_err());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_log_head_that_is_not_the_frame_is_read_as_legacy() {
    let d = dir("fail-log-head");
    let log = d.join("aof-0.aof");
    // a first record that is not one command
    let mut bytes = crate::record::AOF2_MAGIC.to_vec();
    crate::record::write_record(&mut bytes, b"not resp").unwrap();
    std::fs::write(&log, bytes).unwrap();
    assert_eq!(read_log_head(&log).unwrap(), LogHead::Legacy);
    // the frame's verb with an id that is not eight bytes
    let short = Argv::from(vec![LOG_BASE.to_vec(), b"123".to_vec()]);
    assert_eq!(base_of(&short), None);
    log_of(&log, &[short]);
    assert_eq!(read_log_head(&log).unwrap(), LogHead::Legacy);
    assert!(read_log_head(&log.join("under-a-file")).is_err());
    occupied(&d.join("a-directory"));
    assert!(read_log_head(&d.join("a-directory")).is_err());
    assert!(settle_snapshot(&d.join("dump-0.rdb"), Some(&d.join("a-directory"))).is_err());
    std::fs::remove_dir_all(&d).unwrap();
}

/// A kept previous snapshot, with each kind of log beside it: the commit
/// either completed (drop the kept one) or did not (put it back).
#[test]
fn a_kept_snapshot_is_dropped_or_put_back_by_what_the_log_says() {
    let d = dir("fail-prev");
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    let prev = prev_path(&snap);
    // no log, a new snapshot in place: the commit completed
    save_snapshot(&store(&["old"]), &prev).unwrap();
    save_snapshot(&store(&["new"]), &snap).unwrap();
    assert!(settle_snapshot(&snap, None).unwrap());
    assert!(!prev.exists());
    // no log, no snapshot: the kept one goes back
    std::fs::rename(&snap, &prev).unwrap();
    assert!(settle_snapshot(&snap, None).unwrap());
    assert!(snap.exists() && !prev.exists());
    // a complete image beside a kept snapshot: put back, and not loaded
    save_snapshot(&store(&["old"]), &prev).unwrap();
    Aof::open(&log, Fsync::No).unwrap().rewrite_from(&store(&["image"])).unwrap();
    assert!(!settle_snapshot(&snap, Some(&log)).unwrap());
    assert!(!prev.exists());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_kept_snapshot_that_cannot_be_moved_is_an_error() {
    let d = dir("fail-prev-stuck");
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    let prev = prev_path(&snap);
    // the commit completed, but the kept one is a directory unlink refuses
    save_snapshot(&store(&["new"]), &snap).unwrap();
    occupied(&prev);
    assert!(settle_snapshot(&snap, None).is_err());
    std::fs::remove_dir_all(&prev).unwrap();
    std::fs::remove_file(&snap).unwrap();
    // the commit did not complete, and the snapshot's place is taken by a
    // directory the kept one cannot replace
    save_snapshot(&store(&["old"]), &prev).unwrap();
    occupied(&snap);
    log_of(&log, &[]);
    assert!(settle_snapshot(&snap, Some(&log)).is_err());
    std::fs::remove_dir_all(&d).unwrap();
}

/// A log that names a snapshot, beside something that cannot be read as
/// one: the refusal is an error, not a restore.
#[test]
fn a_snapshot_that_cannot_be_read_under_a_log_naming_one_is_an_error() {
    let d = dir("fail-unreadable-snap");
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    log_of(&log, &[base_frame(42)]);
    occupied(&snap);
    assert!(settle_snapshot(&snap, Some(&log)).is_err(), "no kept snapshot");
    occupied(&prev_path(&snap));
    assert!(settle_snapshot(&snap, Some(&log)).is_err(), "with a kept one");
    std::fs::remove_dir_all(&d).unwrap();
}

/// A reshard reads its sources through the same rule: a log whose
/// snapshot is gone stops it.
#[test]
fn a_reshard_refuses_a_log_whose_snapshot_is_gone() {
    let d = dir("fail-reshard");
    log_of(&crate::layout::aof_path(&d, 0), &[base_frame(42)]);
    let mut temp = Store::new();
    let lay = crate::reshard::StdLayout;
    assert!(crate::reshard::merge_sources(&d, 1, &lay, &mut temp, |_, _| {}).is_err());
    std::fs::remove_dir_all(&d).unwrap();
}

/// A begin marker inside an open transaction drops the transaction it
/// interrupts: that one never committed.
#[test]
fn a_begin_inside_a_transaction_drops_the_open_one() {
    let d = dir("fail-nested-begin");
    let log = d.join("aof-0.aof");
    let rpush = |i: &str| Argv::from(vec![b"RPUSH".to_vec(), b"l".to_vec(), i.as_bytes().to_vec()]);
    let begin = Argv::from(vec![Aof::TXN_BEGIN.to_vec()]);
    let commit = Argv::from(vec![Aof::TXN_COMMIT.to_vec()]);
    log_of(&log, &[begin.clone(), rpush("lost"), begin, rpush("kept"), commit]);
    let mut seen = Vec::new();
    replay_aof(&log, |args| seen.push(args[2].to_vec())).unwrap();
    assert_eq!(seen, items(&["kept"]));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A shard's files for a commit test: a snapshot of `a`, a log that
/// continues it with `b`, and the store holding both.
fn committed(name: &str) -> (PathBuf, PathBuf, PathBuf, Aof, Store) {
    let d = dir(name);
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    let mut s = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    push(&mut s, &mut aof, "a");
    let reset = aof.begin_view_rewrite().unwrap();
    let tmp = write_snapshot_tmp(&s, &snap).unwrap();
    aof.commit_snapshot(&tmp, &snap, &reset).unwrap();
    push(&mut s, &mut aof, "b");
    aof.sync_now().unwrap();
    (d, snap, log, aof, s)
}

/// Each failure a commit can meet leaves the snapshot and log it found —
/// a restart restores exactly what the store held — and the log goes on
/// taking writes.
#[test]
fn a_commit_that_fails_leaves_the_pair_it_found() {
    let (d, snap, log, mut aof, mut s) = committed("fail-commit");
    let reset = |aof: &mut Aof| aof.begin_view_rewrite().unwrap();

    // a snapshot without an id
    let r = reset(&mut aof);
    let plain = d.join("plain.rdb");
    std::fs::write(&plain, {
        let mut b = Vec::new();
        crate::write_snapshot_to(&s, &mut b).unwrap();
        b
    })
    .unwrap();
    assert!(aof.commit_snapshot(&plain, &snap, &r).is_err());
    // a snapshot that cannot be read
    let r = reset(&mut aof);
    occupied(&d.join("unreadable"));
    assert!(aof.commit_snapshot(&d.join("unreadable"), &snap, &r).is_err());
    // the snapshot cannot be moved aside: its kept place is taken
    let r = reset(&mut aof);
    let tmp = write_snapshot_tmp(&s, &snap).unwrap();
    occupied(&prev_path(&snap));
    assert!(aof.commit_snapshot(&tmp, &snap, &r).is_err());
    std::fs::remove_dir_all(prev_path(&snap)).unwrap();
    // the log reset cannot be written: the previous snapshot goes back
    let r = reset(&mut aof);
    let tmp = write_snapshot_tmp(&s, &snap).unwrap();
    assert!(aof.commit_snapshot(&tmp, &snap, &d.join("no-such-dir").join("reset")).is_err());
    let _ = r;

    push(&mut s, &mut aof, "c");
    aof.sync_now().unwrap();
    drop(aof);
    assert!(!prev_path(&snap).exists());
    assert_eq!(restore(&snap, &log), items(&["a", "b", "c"]));
    std::fs::remove_dir_all(&d).unwrap();
}

/// The first snapshot of a shard fails at its log reset: it is removed,
/// so the log that stands alone restores alone.
#[test]
fn a_first_snapshot_whose_log_reset_fails_is_removed() {
    let d = dir("fail-first-commit");
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    let mut s = Store::new();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    push(&mut s, &mut aof, "a");
    aof.begin_view_rewrite().unwrap();
    let tmp = write_snapshot_tmp(&s, &snap).unwrap();
    assert!(aof.commit_snapshot(&tmp, &snap, &d.join("no-such-dir").join("reset")).is_err());
    assert!(!snap.exists());
    push(&mut s, &mut aof, "b");
    drop(aof);
    assert_eq!(restore(&snap, &log), items(&["a", "b"]));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A log reset that fails after the live log already names the new
/// snapshot keeps the new snapshot: that pair is the committed one.
#[test]
fn a_reset_that_failed_after_the_log_named_the_snapshot_keeps_it() {
    let d = dir("fail-late-reset");
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    let s = store(&["a"]);
    let tmp = write_snapshot_tmp(&s, &snap).unwrap();
    let id = snapshot_id(&tmp).unwrap().unwrap();
    log_of(&log, &[base_frame(id)]);
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    aof.begin_view_rewrite().unwrap();
    assert!(aof.commit_snapshot(&tmp, &snap, &d.join("no-such-dir").join("reset")).is_err());
    drop(aof);
    assert_eq!(restore(&snap, &log), items(&["a"]));
    // and one that cannot read the live log at all reports that
    let tmp = write_snapshot_tmp(&s, &snap).unwrap();
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    aof.begin_view_rewrite().unwrap();
    std::fs::remove_file(&log).unwrap();
    occupied(&log);
    assert!(aof.commit_snapshot(&tmp, &snap, &d.join("no-such-dir").join("reset")).is_err());
    std::fs::remove_dir_all(&d).unwrap();
}

/// The previous snapshot cannot be put back after a failed reset (its
/// place holds a directory the undo cannot rename over): the error says
/// both, and the restore still refuses rather than load a wrong pair.
#[test]
fn an_undo_that_fails_says_so() {
    let d = dir("fail-undo");
    let (snap, log) = (d.join("dump-0.rdb"), d.join("aof-0.aof"));
    let s = store(&["a"]);
    // the "previous snapshot" is a directory: moving it aside works, and
    // moving it back over the new snapshot file does not
    occupied(&snap);
    let mut aof = Aof::open(&log, Fsync::No).unwrap();
    aof.begin_view_rewrite().unwrap();
    let tmp = write_snapshot_tmp(&s, &d.join("new.rdb")).unwrap();
    let err = aof.commit_snapshot(&tmp, &snap, &d.join("no-such-dir").join("reset")).unwrap_err();
    assert!(err.to_string().contains("restoring the previous snapshot"), "{err}");
    std::fs::remove_dir_all(&d).unwrap();
}

/// A new snapshot on another file system cannot be renamed into place:
/// the previous one goes back.
#[cfg(target_os = "linux")]
#[test]
fn a_snapshot_that_cannot_be_renamed_into_place_puts_the_previous_back() {
    let other = Path::new("/dev/shm");
    if !other.is_dir() {
        return;
    }
    let (d, snap, log, mut aof, s) = committed("fail-exdev");
    let r = aof.begin_view_rewrite().unwrap();
    let away = other.join(format!("kevy-exdev-{}.rdb", std::process::id()));
    let tmp = write_snapshot_tmp(&s, &away).unwrap();
    assert!(aof.commit_snapshot(&tmp, &snap, &r).is_err());
    let _ = std::fs::remove_file(&tmp);
    drop(aof);
    assert!(snap.exists() && !prev_path(&snap).exists());
    assert_eq!(restore(&snap, &log), items(&["a", "b"]));
    // the same with no previous snapshot: nothing to put back
    std::fs::remove_file(&snap).unwrap();
    let mut aof = Aof::open(&d.join("aof-1.aof"), Fsync::No).unwrap();
    let r = aof.begin_view_rewrite().unwrap();
    let tmp = write_snapshot_tmp(&s, &away).unwrap();
    assert!(aof.commit_snapshot(&tmp, &snap, &r).is_err());
    let _ = std::fs::remove_file(&tmp);
    assert!(!snap.exists());
    std::fs::remove_dir_all(&d).unwrap();
}
