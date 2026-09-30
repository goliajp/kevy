//! A log whose file, ring or path fails under it: the failure comes back
//! from the call that met it. A pipe with no reader, or a read-only
//! descriptor of the log itself, stands in for a file that refuses writes.

use crate::tests::{ignore_owed as ignore, temp_file};
use crate::tests_stage_aof::reopen;
use crate::tests_sync::{onto, pipe_file};
use crate::{Aof, Fsync};
use kevy_resp::Argv;
use kevy_store::Store;
use std::path::Path;

/// A `SET k <value>` whose v2 record is exactly `total` bytes: an 8-byte
/// envelope around a multibulk with a three-digit value length.
fn sized(total: usize) -> Argv {
    let v = total - 36;
    assert!((100..1000).contains(&v), "{total} needs a three-digit value");
    Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), vec![b'v'; v]])
}

fn set(size: usize) -> Argv {
    Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), vec![b'v'; size]])
}

fn read_only(path: &Path) -> std::fs::File {
    std::fs::File::open(path).unwrap()
}

#[test]
fn a_full_ring_that_cannot_drain_fails_every_call_that_drains_it() {
    let (path, ring) = (temp_file("fail-full"), temp_file("fail-full-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    for _ in 0..64 {
        log.append(&sized(1024)).unwrap();
    }
    let (pipe, _) = pipe_file(false);
    onto(&mut log, pipe);
    assert!(log.append(&sized(1024)).is_err(), "a record the full ring must drain for");
    log.begin_group();
    assert!(!log.in_txn, "a begin marker that could not be written opened a transaction");
    assert!(log.tick().is_err());
    assert!(log.truncate().is_err());
    assert!(log.begin_concurrent_rewrite(&Store::new()).is_err());
    assert!(log.rewrite_from(&Store::new()).is_err());
}

#[test]
fn a_drain_that_fails_on_the_first_run_skips_the_wrapped_second() {
    let (path, ring) = (temp_file("fail-wrap"), temp_file("fail-wrap-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    for _ in 0..60 {
        log.append(&sized(1024)).unwrap();
    }
    log.tick().unwrap();
    // four fit before the end, the next ones start again at the front
    for _ in 0..8 {
        log.append(&sized(1000)).unwrap();
    }
    let (pipe, _) = pipe_file(false);
    onto(&mut log, pipe);
    assert!(log.tick().is_err());
}

#[test]
fn a_record_that_bypasses_the_ring_fails_when_its_file_does() {
    let (path, ring) = (temp_file("fail-bypass"), temp_file("fail-bypass-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    onto(&mut log, read_only(&path));
    // larger than the ring, smaller than the write buffer: buffered, then
    // pushed into the kernel at once, which the read-only descriptor refuses
    assert!(log.append(&set(100_000)).is_err());
    // the refused bytes are still buffered; the next drain meets them first
    assert!(log.tick().is_err());
    // larger than the write buffer: straight to the file
    let (mut log, _, _) = reopen(&temp_file("fail-bypass-big"), &temp_file("fail-bypass-big-ring"));
    let (pipe, _) = pipe_file(false);
    onto(&mut log, pipe);
    assert!(log.append(&set(300_000)).is_err());
}

#[test]
fn a_commit_marker_after_an_overflowed_transaction_fails_with_its_file() {
    let (path, ring) = (temp_file("fail-overflow"), temp_file("fail-overflow-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    log.begin_group();
    assert!(log.in_txn);
    // larger than the ring: drains it, and the rest of the transaction
    // bypasses the ring
    log.append(&set(100_000)).unwrap();
    let (pipe, _) = pipe_file(false);
    onto(&mut log, pipe);
    assert!(log.end_group().is_err());
}

#[test]
fn a_log_whose_path_is_gone_cannot_name_itself() {
    // a young ring re-takes the log's id over the bytes a drain adds
    let (path, ring) = (temp_file("fail-gone-drain"), temp_file("fail-gone-drain-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    log.append(&set(10)).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(log.tick().is_err());

    // an emptied log rebases the ring onto the id of what is at its path
    let (path, ring) = (temp_file("fail-gone-reset"), temp_file("fail-gone-reset-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(log.truncate().unwrap_err().kind(), std::io::ErrorKind::NotFound);

    // a shorter file at the path cannot supply the bytes the id spans
    let (path, ring) = (temp_file("fail-short-reset"), temp_file("fail-short-reset-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"").unwrap();
    assert_eq!(log.truncate().unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);

    // a ring cannot be set up for a log it cannot identify
    let path = temp_file("fail-gone-stage");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    std::fs::remove_file(&path).unwrap();
    let err = log.open_stage(&temp_file("fail-gone-stage-ring"), 64 * 1024, ignore);
    assert_eq!(err.unwrap_err().kind(), std::io::ErrorKind::NotFound);

    // nor can a ring left by the last process be matched against it
    let (path, ring) = (temp_file("fail-gone-owed"), temp_file("fail-gone-owed-ring"));
    let (log, _, _) = reopen(&path, &ring);
    std::mem::forget(log);
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    std::fs::remove_file(&path).unwrap();
    let err = log.open_stage(&ring, 64 * 1024, ignore).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn a_ring_cannot_be_created_where_its_directory_is_missing() {
    let path = temp_file("fail-ring-nowhere");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    let ring = kevy_tmpdir::unique_dir("fail-ring-nowhere").join("gone").join("ring");
    let err = log.open_stage(&ring, 64 * 1024, ignore).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(log.stage_path(), None);
}

#[test]
fn a_ring_path_that_is_a_directory_is_an_error_not_an_empty_ring() {
    let dir = kevy_tmpdir::TmpDir::new("fail-ring-dir");
    let path = temp_file("fail-ring-dir-log");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    assert!(log.open_stage(dir.path(), 64 * 1024, ignore).is_err());
    assert!(log.settle_stage(dir.path(), ignore).is_err());
    assert!(dir.path().is_dir(), "the directory was left alone");
}

#[test]
fn records_the_ring_owes_fail_to_land_in_a_file_that_refuses_them() {
    let (path, ring) = (temp_file("fail-adopt"), temp_file("fail-adopt-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    log.append(&set(10)).unwrap();
    // killed: the record is committed in the ring and nowhere else
    std::mem::forget(log);
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    onto(&mut log, read_only(&path));
    assert!(log.open_stage(&ring, 64 * 1024, ignore).is_err());
    let magic = crate::record::AOF2_MAGIC.len() as u64;
    assert_eq!(std::fs::metadata(&path).unwrap().len(), magic, "nothing reached the log");
}

#[test]
fn a_ring_left_by_a_longer_log_owes_nothing() {
    let (path, ring) = (temp_file("fail-shrunk"), temp_file("fail-shrunk-ring"));
    let (mut log, _, _) = reopen(&path, &ring);
    log.append(&set(10)).unwrap();
    log.tick().unwrap();
    log.append(&set(20)).unwrap();
    std::mem::forget(log);
    // the log went back to its magic behind the ring's back
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_len(crate::record::AOF2_MAGIC.len() as u64).unwrap();
    let (_log, found, got) = reopen(&path, &ring);
    assert_eq!(found.discarded, Some("the ring continues another log"));
    assert!(got.is_empty());
}

#[test]
fn buffered_bytes_that_cannot_leave_fail_the_open_of_a_ring() {
    let path = temp_file("fail-buffered-stage");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    onto(&mut log, read_only(&path));
    log.append(&set(10)).unwrap();
    assert!(log.open_stage(&temp_file("fail-buffered-stage-ring"), 64 * 1024, ignore).is_err());
}

#[test]
fn a_plain_log_on_a_refusing_file_fails_its_structural_calls() {
    let path = temp_file("fail-plain");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    onto(&mut log, read_only(&path));
    log.append(&set(10)).unwrap();
    assert!(log.truncate().is_err(), "the buffered record cannot be flushed");
    // a fresh writer has nothing to flush; the cut itself is refused
    onto(&mut log, read_only(&path));
    assert!(log.truncate().is_err(), "a read-only descriptor cannot be cut");
    onto(&mut log, read_only(&path));
    log.append(&set(10)).unwrap();
    assert!(log.rewrite_from(&Store::new()).is_err());
    onto(&mut log, read_only(&path));
    log.append(&set(10)).unwrap();
    assert!(log.map_appends().is_err());
    let (pipe, _) = pipe_file(false);
    onto(&mut log, pipe);
    assert!(log.append(&set(300_000)).is_err(), "a record larger than the write buffer");
}

#[test]
fn an_always_log_reports_a_write_or_a_sync_that_fails() {
    let path = temp_file("fail-always");
    let mut log = Aof::open(&path, Fsync::Always).unwrap();
    onto(&mut log, read_only(&path));
    assert!(log.append(&set(10)).is_err(), "the per-append flush is refused");
    let (pipe, _reader) = pipe_file(true);
    onto(&mut log, pipe);
    assert!(log.append(&set(10)).is_err(), "a pipe cannot be synced");
}

#[test]
fn a_rewrite_that_cannot_place_its_output_fails() {
    // the rewrite's temp name is taken by a directory
    let path = temp_file("fail-rw-tmp");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    let tmp = crate::aof_util::rewrite_tmp_path(&path);
    std::fs::create_dir(&tmp).unwrap();
    assert!(log.rewrite_from(&Store::new()).is_err());
    std::fs::remove_dir(&tmp).unwrap();

    // the log's own name is taken by a directory
    let path = temp_file("fail-rw-live");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(log.rewrite_from(&Store::new()).is_err());
    let plan = log.begin_concurrent_rewrite(&Store::new()).unwrap();
    std::fs::write(&plan.tmp, &plan.body).unwrap();
    assert!(log.finish_concurrent_rewrite_with(&plan.tmp, plan.keys, Vec::new()).is_err());
    std::fs::remove_dir(&path).unwrap();

    // the spilled image is gone before the finish
    let path = temp_file("fail-rw-spill");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    let plan = log.begin_concurrent_rewrite(&Store::new()).unwrap();
    let err = log.finish_concurrent_rewrite_with(&plan.tmp, plan.keys, Vec::new()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn reopening_after_a_swap_needs_the_log_at_its_path() {
    let path = temp_file("fail-swap");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    std::fs::remove_file(&path).unwrap();
    let err = log.swap_finalize_reopen(0, None).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn mapping_needs_the_log_at_its_path() {
    let path = temp_file("fail-map");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(log.map_appends().unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn a_mapped_log_has_nothing_to_flush() {
    use std::io::Write;
    let path = temp_file("fail-map-flush");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    assert!(log.map_appends().unwrap());
    log.append(&set(10)).unwrap();
    log.mapped.as_mut().expect("mapped").flush().unwrap();
    log.append(&set(20)).unwrap();
    drop(log);
    let mut n = 0;
    crate::replay_aof(&path, |_| n += 1).unwrap();
    assert_eq!(n, 2, "the records on both sides of the flush");
}

#[test]
fn a_directory_is_not_a_log() {
    let dir = kevy_tmpdir::TmpDir::new("fail-log-dir");
    assert!(Aof::open(dir.path(), Fsync::EverySec).is_err());
}

#[test]
fn a_transaction_on_a_v1_log_writes_no_markers() {
    let path = temp_file("fail-v1-txn");
    std::fs::write(&path, crate::aof::AOF_MAGIC).unwrap();
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    log.begin_group();
    assert!(log.in_txn, "a v1 log still brackets its group");
    log.end_group().unwrap();
    log.sync_now().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), crate::aof::AOF_MAGIC, "no marker was written");
}

#[test]
fn markers_written_during_a_rewrite_reach_its_diff() {
    let path = temp_file("fail-tee-markers");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    let plan = log.begin_concurrent_rewrite(&Store::new()).unwrap();
    log.begin_group();
    log.append(&set(10)).unwrap();
    log.end_group().unwrap();
    let tee = log.rewrite_tee.as_ref().expect("teeing");
    let has = |m: &[u8]| tee.windows(m.len()).any(|w| w == m);
    assert!(has(Aof::TXN_BEGIN) && has(Aof::TXN_COMMIT));
    std::fs::write(&plan.tmp, &plan.body).unwrap();
    log.finish_concurrent_rewrite_with(&plan.tmp, plan.keys, Vec::new()).unwrap();
}
