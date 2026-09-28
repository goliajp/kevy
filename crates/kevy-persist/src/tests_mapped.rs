//! A log that maps its appends against one that writes them, and what a
//! killed process leaves behind. `mem::forget` stands in for the kill.

use crate::tests::temp_file;
use crate::{Aof, Fsync, replay_aof};
use kevy_resp::Argv;

fn set(i: usize, size: usize) -> Argv {
    Argv::from(vec![b"SET".to_vec(), format!("k{i}").into_bytes(), vec![b'v'; size]])
}

fn keys(path: &std::path::Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    replay_aof(path, |a| out.push(a[1].to_vec())).unwrap();
    out
}

fn mapped(path: &std::path::Path) -> Aof {
    let mut log = Aof::open(path, Fsync::EverySec).unwrap();
    assert!(log.map_appends().unwrap());
    log
}

fn run(log: &mut Aof) {
    // past the first 4 MiB chunk, so a record straddles two mappings
    for i in 0..1400 {
        log.append(&set(i, 3000 + i % 7)).unwrap();
        if i % 300 == 0 {
            let _ = log.tick().unwrap();
        }
    }
    log.begin_group();
    log.append(&set(5000, 10)).unwrap();
    log.end_group().unwrap();
    log.truncate().unwrap();
    for i in 0..50 {
        log.append(&set(i, 100)).unwrap();
    }
    let mut store = kevy_store::Store::new();
    store.set(b"kept", b"v".to_vec(), None, false, false);
    log.rewrite_from(&store).unwrap();
    log.append(&set(99, 10)).unwrap();
    log.sync_now().unwrap();
}

#[test]
fn a_mapped_log_holds_the_same_bytes_as_a_written_one() {
    let (plain, maps) = (temp_file("mapped-plain"), temp_file("mapped-maps"));
    let mut p = Aof::open(&plain, Fsync::EverySec).unwrap();
    run(&mut p);
    drop(p);
    let mut m = mapped(&maps);
    run(&mut m);
    assert!(m.maps_appends(), "still mapping after the swaps");
    drop(m);
    assert_eq!(std::fs::read(&plain).unwrap(), std::fs::read(&maps).unwrap());
}

#[test]
fn a_killed_process_leaves_a_zero_tail_and_every_record() {
    let path = temp_file("mapped-kill");
    let mut log = mapped(&path);
    for i in 0..20 {
        log.append(&set(i, 500)).unwrap();
    }
    std::mem::forget(log);
    let written = std::fs::metadata(&path).unwrap().len();
    let r = replay_aof(&path, |_| {}).unwrap();
    assert_eq!((r.commands, r.dropped_bytes), (20, 0));
    assert!(r.zero_tail > 0 && r.replayed_bytes + r.zero_tail == written);
    let log = Aof::open(&path, Fsync::EverySec).unwrap();
    assert!(log.open_quarantine().is_none());
    drop(log);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), r.replayed_bytes);
}

#[test]
fn an_off_lock_sync_runs_while_appends_go_on() {
    let path = temp_file("mapped-sync");
    let mut log = mapped(&path);
    log.append(&set(0, 10)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let pending = log.tick().unwrap().expect("the everysec window elapsed");
    let syncer = std::thread::spawn(move || pending.run());
    for i in 1..3000 {
        log.append(&set(i, 2000)).unwrap(); // grows past the first chunk meanwhile
    }
    syncer.join().unwrap().unwrap();
    drop(log);
    assert_eq!(keys(&path).len(), 3000);
}

#[test]
fn switching_to_always_goes_back_to_write_with_no_hole() {
    let path = temp_file("mapped-always");
    let mut log = mapped(&path);
    log.append(&set(0, 10)).unwrap();
    log.set_fsync(Fsync::Always).unwrap();
    assert!(!log.maps_appends());
    log.append(&set(1, 10)).unwrap();
    std::mem::forget(log);
    assert_eq!(keys(&path), [b"k0".to_vec(), b"k1".to_vec()]);
}

#[test]
fn a_log_that_stages_or_speaks_v1_does_not_map() {
    let path = temp_file("mapped-staged");
    let mut log = Aof::open(&path, Fsync::EverySec).unwrap();
    log.open_stage(&temp_file("mapped-staged-ring"), 64 * 1024, |_| {}).unwrap();
    assert!(!log.map_appends().unwrap());
    let v1 = temp_file("mapped-v1");
    std::fs::write(&v1, b"KEVYAOF1\n").unwrap();
    let mut log = Aof::open(&v1, Fsync::EverySec).unwrap();
    assert!(!log.map_appends().unwrap());
    let mut log = Aof::open(&temp_file("mapped-always-open"), Fsync::Always).unwrap();
    assert!(!log.map_appends().unwrap());
}

#[test]
fn records_after_a_truncate_are_the_log() {
    let path = temp_file("mapped-truncate");
    let mut log = mapped(&path);
    for i in 0..10 {
        log.append(&set(i, 100)).unwrap();
    }
    log.truncate().unwrap();
    for i in 20..23 {
        log.append(&set(i, 100)).unwrap();
    }
    assert!(log.maps_appends());
    std::mem::forget(log);
    assert_eq!(keys(&path), [b"k20".to_vec(), b"k21".to_vec(), b"k22".to_vec()]);
}
