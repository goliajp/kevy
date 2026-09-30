//! A mapped log ends in zeros: the part of its preallocation no record
//! reached. Replay stops there cleanly and the open cuts it off without
//! quarantine; zeros with data after them are still damage.

use crate::tests::temp_file;
use crate::{Aof, Fsync, replay_aof};
use kevy_resp::Argv;

fn set(i: usize) -> Argv {
    Argv::from(vec![b"SET".to_vec(), format!("k{i}").into_bytes(), b"v".to_vec()])
}

fn log_of(n: usize, name: &str) -> (std::path::PathBuf, u64) {
    let path = temp_file(name);
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    for i in 0..n {
        aof.append(&set(i)).unwrap();
    }
    aof.sync_now().unwrap();
    drop(aof);
    let len = std::fs::metadata(&path).unwrap().len();
    (path, len)
}

#[test]
fn a_zero_tail_ends_the_log_cleanly_and_is_cut_without_quarantine() {
    for extra in [3u64, 8, 4 << 20] {
        let (path, len) = log_of(5, &format!("zero-tail-{extra}"));
        std::fs::File::options().write(true).open(&path).unwrap().set_len(len + extra).unwrap();
        let mut n = 0;
        let r = replay_aof(&path, |_| n += 1).unwrap();
        assert_eq!(
            (n, r.dropped_bytes, r.zero_tail, r.corrupt),
            (5, 0, extra, false),
            "tail {extra}"
        );
        assert_eq!(r.replayed_bytes, len);
        let aof = Aof::open(&path, Fsync::No).unwrap();
        assert!(aof.open_quarantine().is_none(), "zeros are not data");
        drop(aof);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len, "the tail is cut off");
    }
}

#[test]
fn the_settled_length_cuts_a_zero_tail_without_a_second_walk() {
    let (path, len) = log_of(3, "zero-tail-settled");
    std::fs::File::options().write(true).open(&path).unwrap().set_len(len + 4096).unwrap();
    let aof =
        Aof::open_after_replay(&path, Fsync::No, crate::ReplayMode::Strict, Some(len)).unwrap();
    assert!(aof.open_quarantine().is_none());
    drop(aof);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
}

#[test]
fn zeros_with_records_after_them_are_damage() {
    use std::os::unix::fs::FileExt;
    let (path, len) = log_of(3, "zero-hole");
    let (tail, _) = log_of(1, "zero-hole-tail");
    let rec = std::fs::read(&tail).unwrap()[crate::AOF2_MAGIC.len()..].to_vec();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_len(len + 64).unwrap();
    f.write_all_at(&rec, len + 64).unwrap();
    let mut n = 0;
    let r = replay_aof(&path, |_| n += 1).unwrap();
    assert_eq!((n, r.zero_tail), (3, 0));
    assert!(r.corrupt && r.dropped_bytes > 0, "a hole is not a tail: {r:?}");
}
