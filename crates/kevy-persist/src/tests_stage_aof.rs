//! A staged log against a plain one, and what a killed process leaves.
//! `mem::forget` stands in for the kill: no destructor runs, nothing is
//! drained, and the ring's pages stay exactly as the last append left them.

use crate::stage_ring::StageRing;
use crate::tests::temp_file;
use crate::{Aof, Fsync, replay_aof};
use kevy_resp::Argv;

const CAP: u64 = 64 * 1024;

fn cmd(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

fn set(i: usize, size: usize) -> Argv {
    cmd(&[b"SET", format!("k{i}").as_bytes(), &vec![b'v'; size]])
}

fn words(a: &Argv) -> Vec<Vec<u8>> {
    (0..a.len()).map(|i| a[i].to_vec()).collect()
}

fn replayed(path: &std::path::Path) -> Vec<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    replay_aof(path, |a| out.push(words(&a))).unwrap();
    out
}

/// Open the log and its ring the way a store does, collecting what the
/// ring hands back.
pub(crate) fn reopen(
    aof: &std::path::Path,
    ring: &std::path::Path,
) -> (Aof, crate::StageOpen, Vec<Argv>) {
    let mut log = Aof::open(aof, Fsync::EverySec).unwrap();
    let mut got = Vec::new();
    let found = log.open_stage(ring, CAP, |a| got.push(std::mem::take(a))).unwrap();
    (log, found, got)
}

fn run(log: &mut Aof) {
    for i in 0..40 {
        log.append(&set(i, 300)).unwrap();
        if i % 7 == 0 {
            let _ = log.tick().unwrap();
        }
    }
    log.begin_group();
    log.append(&set(100, 10)).unwrap();
    log.append(&set(101, 10)).unwrap();
    log.end_group().unwrap();
    log.truncate().unwrap();
    for i in 200..260 {
        log.append(&set(i, 2000)).unwrap();
    }
    log.sync_now().unwrap();
}

#[test]
fn a_staged_log_writes_the_same_bytes_as_a_plain_one() {
    let (plain, staged) = (temp_file("stage-aof-plain"), temp_file("stage-aof-staged"));
    let mut p = Aof::open(&plain, Fsync::EverySec).unwrap();
    run(&mut p);
    let (mut s, found, _) = reopen(&staged, &temp_file("stage-aof-ring"));
    assert_eq!(found, crate::StageOpen::default());
    assert!(s.stage_path().is_some());
    run(&mut s);
    assert_eq!(std::fs::read(&plain).unwrap(), std::fs::read(&staged).unwrap());
}

#[test]
fn a_killed_process_keeps_every_staged_append() {
    let (aof, ring) = (temp_file("stage-kill-aof"), temp_file("stage-kill-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    log.append(&set(0, 50)).unwrap();
    let _ = log.tick().unwrap(); // drained: in the file
    for i in 1..6 {
        log.append(&set(i, 50)).unwrap(); // staged only
    }
    std::mem::forget(log);
    assert_eq!(replayed(&aof).len(), 1, "only the drained record reached the file");
    let (log, found, got) = reopen(&aof, &ring);
    assert_eq!(found.recovered, 5);
    assert_eq!(
        got.iter().map(words).collect::<Vec<_>>(),
        (1..6).map(|i| words(&set(i, 50))).collect::<Vec<_>>()
    );
    drop(log);
    assert_eq!(replayed(&aof).len(), 6, "the recovered records were appended to the file");
    // and a second open owes nothing more
    let (_, again, got) = reopen(&aof, &ring);
    assert_eq!((again.recovered, got.len()), (0, 0));
}

#[test]
fn a_copied_directory_keeps_every_staged_append() {
    let (aof, ring) = (temp_file("stage-copy-aof"), temp_file("stage-copy-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    log.append(&set(0, 50)).unwrap();
    let _ = log.tick().unwrap();
    for i in 1..6 {
        log.append(&set(i, 50)).unwrap();
    }
    std::mem::forget(log);
    // a copy is a new file: another inode, the same bytes
    let (aof2, ring2) = (temp_file("stage-copy-aof2"), temp_file("stage-copy-ring2"));
    std::fs::copy(&aof, &aof2).unwrap();
    std::fs::copy(&ring, &ring2).unwrap();
    let (_, found, got) = reopen(&aof2, &ring2);
    assert_eq!((found.recovered, got.len(), found.discarded), (5, 5, None));
    assert_eq!(replayed(&aof2).len(), 6);
}

#[test]
fn a_log_of_the_same_length_but_other_bytes_is_not_the_rings() {
    let (aof, ring) = (temp_file("stage-other-aof"), temp_file("stage-other-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    log.append(&cmd(&[b"SET", b"k", b"vvvv"])).unwrap();
    let _ = log.tick().unwrap();
    log.append(&set(1, 50)).unwrap();
    std::mem::forget(log);
    let before = std::fs::metadata(&aof).unwrap().len();
    std::fs::remove_file(&aof).unwrap();
    let mut other = Aof::open(&aof, Fsync::EverySec).unwrap();
    other.append(&cmd(&[b"SET", b"k", b"wwww"])).unwrap();
    other.sync_now().unwrap();
    drop(other);
    assert_eq!(std::fs::metadata(&aof).unwrap().len(), before);
    let (_, found, got) = reopen(&aof, &ring);
    assert_eq!((found.recovered, got.len()), (0, 0));
    assert!(found.discarded.is_some());
}

#[test]
fn a_kill_between_a_drain_and_its_header_is_not_replayed_twice() {
    let (aof, ring) = (temp_file("stage-half-aof"), temp_file("stage-half-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    let before = StageRing::open_existing(&ring).unwrap().unwrap().1;
    for i in 0..4 {
        log.append(&set(i, 50)).unwrap();
    }
    let _ = log.tick().unwrap();
    std::mem::forget(log);
    // put the header back as it was before the drain moved it
    let (mut r, _) = StageRing::open_existing(&ring).unwrap().unwrap();
    r.mark_drained(before.drained, before.aof_len, before.log_id);
    drop(r);
    let (_, found, got) = reopen(&aof, &ring);
    assert_eq!((found.recovered, got.len(), found.discarded), (0, 0, None));
    assert_eq!(replayed(&aof).len(), 4);
}

#[test]
fn an_uncommitted_transaction_in_the_ring_is_dropped_and_leaves_the_log_whole() {
    let (aof, ring) = (temp_file("stage-txn-aof"), temp_file("stage-txn-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    log.append(&set(0, 10)).unwrap();
    log.begin_group();
    log.append(&set(1, 10)).unwrap();
    std::mem::forget(log);
    let (mut log, found, got) = reopen(&aof, &ring);
    assert_eq!(found.recovered, 1);
    assert_eq!(got.iter().map(words).collect::<Vec<_>>(), [words(&set(0, 10))]);
    log.append(&set(2, 10)).unwrap();
    log.sync_now().unwrap();
    drop(log);
    assert_eq!(replayed(&aof), [words(&set(0, 10)), words(&set(2, 10))]);
}

#[test]
fn a_transaction_larger_than_the_ring_lands_whole_in_the_file() {
    let (aof, ring) = (temp_file("stage-big-aof"), temp_file("stage-big-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    log.begin_group();
    for i in 0..60 {
        log.append(&set(i, 2000)).unwrap(); // ~120 KiB against a 64 KiB ring
    }
    log.end_group().unwrap();
    log.append(&set(99, 10)).unwrap();
    std::mem::forget(log);
    let (log, found, got) = reopen(&aof, &ring);
    assert_eq!((found.recovered, got.len()), (1, 1), "only the record after the transaction");
    drop(log);
    let all = replayed(&aof);
    assert_eq!(all.len(), 61);
    assert_eq!(all[59], words(&set(59, 2000)));
}

#[test]
fn after_a_rewrite_the_ring_owes_only_what_came_after_it() {
    let (aof, ring) = (temp_file("stage-rw-aof"), temp_file("stage-rw-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    let mut store = kevy_store::Store::new();
    for i in 0..5 {
        log.append(&set(i, 20)).unwrap();
        store.set(
            format!("k{i}").as_bytes(),
            vec![b'v'; 20],
            None,
            kevy_store::SetCondition::Always,
        );
    }
    log.rewrite_from(&store).unwrap();
    log.append(&set(7, 20)).unwrap();
    std::mem::forget(log);
    let (_, found, got) = reopen(&aof, &ring);
    assert_eq!(found.recovered, 1);
    assert_eq!(got.iter().map(words).collect::<Vec<_>>(), [words(&set(7, 20))]);
}

#[test]
fn a_record_larger_than_the_ring_goes_to_the_file_and_survives_a_kill() {
    let (aof, ring) = (temp_file("stage-huge-aof"), temp_file("stage-huge-ring"));
    let (mut log, _, _) = reopen(&aof, &ring);
    log.append(&set(0, 70 * 1024)).unwrap(); // larger than the 64 KiB ring
    log.append(&set(1, 10)).unwrap(); // staged after it
    std::mem::forget(log);
    let (log, found, got) = reopen(&aof, &ring);
    assert_eq!((found.recovered, found.discarded), (1, None));
    assert_eq!(got.iter().map(words).collect::<Vec<_>>(), [words(&set(1, 10))]);
    drop(log);
    assert_eq!(replayed(&aof), [words(&set(0, 70 * 1024)), words(&set(1, 10))]);
}
