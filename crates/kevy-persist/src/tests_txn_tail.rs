//! A log that ends inside a transaction. Replay drops the uncommitted
//! records; the open that follows must cut them off too, or the next
//! session's appends land after an open begin marker and the next replay
//! drops them along with it.

use crate::tests::temp_file;
use crate::{Aof, Fsync, replay_aof};
use kevy_resp::Argv;

fn set(i: usize) -> Argv {
    Argv::from(vec![b"SET".to_vec(), format!("k{i}").into_bytes(), b"v".to_vec()])
}

fn keys(path: &std::path::Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    replay_aof(path, |a| out.push(a[1].to_vec())).unwrap();
    out
}

#[test]
fn writes_after_a_crash_inside_a_transaction_survive_the_next_replay() {
    let aof = temp_file("txn-tail");
    let mut log = Aof::open(&aof, Fsync::EverySec).unwrap();
    log.append(&set(0)).unwrap();
    log.begin_group();
    log.append(&set(1)).unwrap();
    let _ = log.tick().unwrap(); // the half transaction reaches the file
    std::mem::forget(log); // the process dies before the commit marker
    let report = replay_aof(&aof, |_| {}).unwrap();
    assert!(report.dropped_bytes > 0, "the open transaction is not part of the settled log");
    let mut log = Aof::open(&aof, Fsync::EverySec).unwrap();
    assert!(log.open_quarantine().is_some(), "the cut records are kept aside");
    log.append(&set(2)).unwrap();
    log.sync_now().unwrap();
    drop(log);
    assert_eq!(keys(&aof), [b"k0".to_vec(), b"k2".to_vec()]);
}

#[test]
fn a_committed_transaction_at_the_end_is_kept() {
    let aof = temp_file("txn-tail-committed");
    let mut log = Aof::open(&aof, Fsync::EverySec).unwrap();
    log.begin_group();
    log.append(&set(1)).unwrap();
    log.end_group().unwrap();
    let _ = log.tick().unwrap();
    std::mem::forget(log);
    let report = replay_aof(&aof, |_| {}).unwrap();
    assert_eq!(report.dropped_bytes, 0);
    let mut log = Aof::open(&aof, Fsync::EverySec).unwrap();
    assert!(log.open_quarantine().is_none());
    log.append(&set(2)).unwrap();
    log.sync_now().unwrap();
    drop(log);
    assert_eq!(keys(&aof), [b"k1".to_vec(), b"k2".to_vec()]);
}
