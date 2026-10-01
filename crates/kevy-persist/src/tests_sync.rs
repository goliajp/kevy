//! The per-tick half of the fsync policy: `No` empties the buffer into
//! the kernel, `EverySec` hands its fsync back to the caller.

use super::*;
use crate::tests::temp_file;
use std::time::{Duration, Instant};

fn set(k: &[u8]) -> Argv {
    Argv::from(vec![b"SET".to_vec(), k.to_vec(), vec![b'v'; 64]])
}

fn on_disk(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

fn window_elapsed(aof: &mut Aof) {
    aof.last_sync = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
}

#[test]
fn no_tick_writes_the_buffer_into_the_kernel() {
    let path = temp_file("sync-no");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    let header = on_disk(&path);
    aof.append(&set(b"a")).unwrap();
    assert_eq!(on_disk(&path), header, "the record is still in the user-space buffer");
    assert!(aof.tick().unwrap().is_none(), "no mode never hands out an fsync");
    assert!(on_disk(&path) > header, "tick must write the buffer into the kernel");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn always_tick_owes_nothing() {
    // every append already reached the disk, so the tick has no sync to hand out
    let path = temp_file("sync-always");
    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    let header = on_disk(&path);
    aof.append(&set(b"a")).unwrap();
    assert!(on_disk(&path) > header, "always writes through on append");
    assert!(aof.tick().unwrap().is_none());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn no_maybe_sync_writes_the_buffer_into_the_kernel() {
    // the server's synchronous reactor path ticks through maybe_sync
    let path = temp_file("sync-no-maybe");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    let header = on_disk(&path);
    aof.append(&set(b"a")).unwrap();
    aof.maybe_sync().unwrap();
    assert!(on_disk(&path) > header);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn everysec_tick_writes_the_buffer_inside_the_window() {
    let path = temp_file("sync-everysec-write");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    let header = on_disk(&path);
    aof.append(&set(b"a")).unwrap();
    assert!(aof.tick().unwrap().is_none(), "the fsync window has not elapsed");
    assert!(on_disk(&path) > header, "the tick still writes the buffer into the kernel");
    assert!(aof.dirty, "written is not synced: the next due tick still owes the fsync");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn everysec_tick_waits_for_the_window() {
    let path = temp_file("sync-window");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    aof.append(&set(b"a")).unwrap();
    assert!(aof.tick().unwrap().is_none(), "inside the window nothing is due");
    window_elapsed(&mut aof);
    assert!(aof.tick().unwrap().is_some());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn everysec_sync_runs_without_the_log_and_covers_the_flushed_prefix() {
    let path = temp_file("sync-off");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    aof.append(&set(b"a")).unwrap();
    window_elapsed(&mut aof);
    let pending = aof.tick().unwrap().expect("due");
    let covered = on_disk(&path);
    assert!(covered > AOF2_MAGIC.len() as u64, "the record reached the kernel before the handle");
    // the log is free while the sync is outstanding
    aof.append(&set(b"b")).unwrap();
    assert!(aof.sync_unconfirmed());
    pending.run().unwrap();
    assert!(!aof.sync_unconfirmed());
    drop(aof);
    let mut got = Vec::new();
    replay_aof(&path, |a| got.push(a)).unwrap();
    assert_eq!(got, vec![set(b"a"), set(b"b")]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn barrier_does_not_trust_dirty_while_a_sync_is_outstanding() {
    // tick cleared `dirty`; a barrier that returned early here would
    // claim durability the outstanding fsync has not delivered
    let path = temp_file("sync-barrier");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    aof.append(&set(b"a")).unwrap();
    window_elapsed(&mut aof);
    let pending = aof.tick().unwrap().expect("due");
    assert!(!aof.dirty);
    aof.sync_now().unwrap();
    assert!(!aof.sync_unconfirmed(), "sync_now must sync and confirm");
    pending.run().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn upgrade_to_always_syncs_while_a_sync_is_outstanding() {
    let path = temp_file("sync-upgrade");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    aof.append(&set(b"a")).unwrap();
    window_elapsed(&mut aof);
    let pending = aof.tick().unwrap().expect("due");
    aof.set_fsync(Fsync::Always).unwrap();
    assert!(!aof.sync_unconfirmed(), "the upgrade must sync what the tick left unconfirmed");
    drop(pending);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_unconfirmed_sync_is_retried_on_the_next_tick() {
    // a failed fsync leaves its records unconfirmed; the next tick
    // retries at once instead of waiting out a fresh window
    let path = temp_file("sync-retry");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    aof.append(&set(b"a")).unwrap();
    window_elapsed(&mut aof);
    drop(aof.tick().unwrap().expect("due"));
    let retry = aof.tick().unwrap().expect("an unconfirmed sync is due again");
    retry.run().unwrap();
    assert!(aof.tick().unwrap().is_none(), "confirmed and clean: nothing due");
    let _ = std::fs::remove_file(&path);
}

// a pipe stands in for a failing log: writes fail once the read end is
// gone, and no platform can fsync a pipe
#[cfg(unix)]
pub(crate) fn pipe_file(keep_reader: bool) -> (std::fs::File, Option<std::io::PipeReader>) {
    let (r, w) = std::io::pipe().unwrap();
    let file = std::fs::File::from(std::os::fd::OwnedFd::from(w));
    (file, keep_reader.then_some(r))
}

#[cfg(unix)]
pub(crate) fn onto(aof: &mut Aof, file: std::fs::File) {
    aof.file = std::io::BufWriter::with_capacity(crate::aof::AOF_BUF_CAP, file);
}

#[cfg(unix)]
#[test]
fn a_failed_fsync_leaves_the_log_unconfirmed() {
    let path = temp_file("sync-fail");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    let (pipe, _reader) = pipe_file(true);
    onto(&mut aof, pipe);
    aof.append(&set(b"a")).unwrap();
    window_elapsed(&mut aof);
    let pending = aof.tick().unwrap().expect("due");
    assert!(pending.run().is_err(), "a pipe cannot be fsynced");
    assert!(aof.sync_unconfirmed(), "a failed fsync must not confirm");
    assert!(aof.tick().unwrap().is_some(), "and the next tick retries it");
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[test]
fn maybe_sync_reports_a_failed_fsync() {
    let path = temp_file("sync-fail-inline");
    let mut aof = Aof::open(&path, Fsync::EverySec).unwrap();
    let (pipe, _reader) = pipe_file(true);
    onto(&mut aof, pipe);
    aof.append(&set(b"a")).unwrap();
    window_elapsed(&mut aof);
    assert!(aof.maybe_sync().is_err());
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[test]
fn a_failed_write_keeps_the_records_dirty() {
    for fsync in [Fsync::No, Fsync::EverySec] {
        let path = temp_file("sync-write-fail");
        let mut aof = Aof::open(&path, fsync).unwrap();
        let (pipe, _) = pipe_file(false);
        onto(&mut aof, pipe);
        aof.append(&set(b"a")).unwrap();
        window_elapsed(&mut aof);
        assert!(aof.tick().is_err(), "{fsync:?}: the write into a closed pipe fails");
        assert!(aof.maybe_sync().is_err(), "{fsync:?}");
        assert!(aof.dirty, "{fsync:?}: nothing reached the kernel, nothing may be cleared");
        assert!(!aof.sync_unconfirmed(), "{fsync:?}: no sync was handed out");
        let _ = std::fs::remove_file(&path);
    }
}
