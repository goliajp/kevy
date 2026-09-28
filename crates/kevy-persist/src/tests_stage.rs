//! The staging ring and its recovery verdicts.

use crate::record::write_record;
use crate::stage_recover::{Recovery, recover};
use crate::stage_ring::{HEADER, StageRing};
use crate::tests::temp_file;

const CAP: u64 = 64 * 1024;

fn record(payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    write_record(&mut v, payload).unwrap();
    v
}

fn push(ring: &mut StageRing, rec: &[u8]) -> bool {
    ring.push(rec.len(), |slot| slot.copy_from_slice(rec))
}

fn pending(ring: &StageRing) -> Vec<u8> {
    let mut out = Vec::new();
    ring.for_each_pending(|run| out.extend_from_slice(run));
    out
}

#[test]
fn pushed_records_come_back_in_order_as_one_run() {
    let mut ring = StageRing::create(&temp_file("stage-order"), CAP, 7, 9).unwrap();
    let (a, b) = (record(b"first"), record(b"second"));
    assert!(push(&mut ring, &a) && push(&mut ring, &b));
    let mut runs = 0;
    let end = ring.for_each_pending(|_| runs += 1);
    assert_eq!(runs, 1);
    assert_eq!(pending(&ring), [a.clone(), b.clone()].concat());
    assert_eq!(end, (a.len() + b.len()) as u64);
    assert_eq!(ring.head().commit, end);
}

// fill to just short of the end, then push a record that cannot fit there
#[test]
fn a_record_that_would_straddle_the_end_starts_again_at_the_front() {
    use std::os::unix::fs::FileExt;
    for gap in [20u64, 5] {
        let path = temp_file(&format!("stage-wrap-{gap}"));
        let mut ring = StageRing::create(&path, CAP, 1, 9).unwrap();
        let filler = record(&vec![b'f'; (CAP - gap) as usize - 8]);
        assert!(push(&mut ring, &filler));
        ring.mark_drained(ring.head().commit, 100, ring.head().log_id);
        // what an earlier lap left in the gap, written through the file,
        // which the shared mapping sees
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.write_all_at(&vec![0xff; gap as usize], HEADER as u64 + CAP - gap).unwrap();
        let rec = record(b"after the wrap");
        assert!(rec.len() as u64 > gap, "the record must not fit in the gap");
        assert!(push(&mut ring, &rec));
        assert_eq!(pending(&ring), rec, "gap {gap}");
        assert_eq!(ring.head().commit, CAP + rec.len() as u64);
    }
}

#[test]
fn a_full_ring_refuses_until_it_is_drained() {
    let mut ring = StageRing::create(&temp_file("stage-full"), CAP, 1, 9).unwrap();
    let rec = record(&[b'x'; 1000]);
    let mut n = 0;
    while push(&mut ring, &rec) {
        n += 1;
    }
    assert_eq!(n, CAP as usize / rec.len());
    let end = ring.for_each_pending(|_| {});
    ring.mark_drained(end, 9, ring.head().log_id);
    assert!(push(&mut ring, &rec));
    assert!(!ring.push(CAP as usize + 1, |_| unreachable!()), "a record larger than the ring");
}

#[test]
fn an_existing_ring_reopens_as_it_was_left() {
    let path = temp_file("stage-reopen");
    let mut ring = StageRing::create(&path, CAP, 42, 900).unwrap();
    let rec = record(b"kept");
    assert!(push(&mut ring, &rec));
    let head = ring.head();
    drop(ring);
    let (again, found) = StageRing::open_existing(&path).unwrap().unwrap();
    assert_eq!(found, head);
    assert_eq!((found.log_id, found.aof_len), (42, 900));
    assert_eq!(pending(&again), rec);
    std::fs::write(&path, b"not a ring").unwrap();
    assert!(StageRing::open_existing(&path).unwrap().is_none());
    assert!(StageRing::open_existing(&temp_file("stage-none")).unwrap().is_none());
}

fn ring_with(name: &str, recs: &[Vec<u8>]) -> StageRing {
    let mut ring = StageRing::create(&temp_file(name), CAP, 5, 100).unwrap();
    for r in recs {
        assert!(push(&mut ring, r));
    }
    ring
}

#[test]
fn recovery_replays_what_the_log_does_not_hold() {
    let recs = [record(b"a"), record(b"bb"), record(b"ccc")];
    let ring = ring_with("stage-rec-all", &recs);
    let head = ring.head();
    // nothing drained: all three come back
    assert_eq!(
        recover(&ring, head, 5, 100, &[]),
        Recovery::Replay { records: recs.to_vec(), torn: false }
    );
    // the first reached the log but the header was not moved
    assert_eq!(
        recover(&ring, head, 5, 100 + recs[0].len() as u64, &recs[0]),
        Recovery::Replay { records: recs[1..].to_vec(), torn: false }
    );
    // all of them reached the log
    let all = recs.concat();
    assert_eq!(
        recover(&ring, head, 5, 100 + all.len() as u64, &all),
        Recovery::Replay { records: vec![], torn: false }
    );
}

#[test]
fn recovery_discards_a_ring_that_cannot_prove_its_place() {
    let recs = [record(b"a"), record(b"bb")];
    let ring = ring_with("stage-rec-discard", &recs);
    let head = ring.head();
    let discarded = |r: Recovery| matches!(r, Recovery::Discard(_));
    assert!(discarded(recover(&ring, head, 6, 100, &[])), "another log");
    assert!(discarded(recover(&ring, head, 5, 99, &[])), "a shorter log");
    let other = record(b"z"); // as long as the first record, different bytes
    assert!(discarded(recover(&ring, head, 5, 100 + other.len() as u64, &other)), "a foreign tail");
    let long = [recs.concat(), record(b"more")].concat();
    assert!(discarded(recover(&ring, head, 5, 100 + long.len() as u64, &long)), "a longer log");
}

#[test]
fn recovery_stops_at_a_record_that_fails_its_checksum() {
    let path = temp_file("stage-rec-torn");
    let recs = [record(b"good"), record(b"flipped"), record(b"after")];
    let mut ring = StageRing::create(&path, CAP, 5, 100).unwrap();
    for r in &recs {
        assert!(push(&mut ring, r));
    }
    let head = ring.head();
    drop(ring);
    // flip a payload byte of the second record on disk
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[HEADER + recs[0].len() + 8] ^= 0x20;
    std::fs::write(&path, &bytes).unwrap();
    let (ring, found) = StageRing::open_existing(&path).unwrap().unwrap();
    assert_eq!(found, head);
    assert_eq!(
        recover(&ring, found, 5, 100, &[]),
        Recovery::Replay { records: recs[..1].to_vec(), torn: true }
    );
}

#[test]
fn a_state_written_but_not_selected_is_not_the_head() {
    let path = temp_file("stage-select");
    let mut ring = StageRing::create(&path, CAP, 5, 100).unwrap();
    assert!(push(&mut ring, &record(b"one")));
    ring.mark_drained(ring.head().commit, 140, ring.head().log_id);
    let head = ring.head();
    ring.write_unselected([5, 999, 0]);
    drop(ring);
    let (_, found) = StageRing::open_existing(&path).unwrap().unwrap();
    assert_eq!(found, head, "the half-written slot must not be read");
    assert_eq!((found.aof_len, found.drained), (140, head.commit));
}
