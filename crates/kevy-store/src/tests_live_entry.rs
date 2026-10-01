//! The single-probe read path (`live_entry` / `live_entry_mut`, under GET
//! and INCR): what a hit, a miss and an expired key each leave behind.

use core::time::Duration;

use crate::{EvictionPolicy, RowChanges, RowWatch, SetCondition, Store};

fn expired_key(s: &mut Store, key: &[u8], value: &[u8]) {
    s.set(key, value.to_vec(), Some(Duration::from_millis(1)), SetCondition::Always);
    std::thread::sleep(Duration::from_millis(5));
}

#[test]
fn a_read_of_an_expired_key_drops_it_and_counts_it() {
    let mut s = Store::new();
    s.set(b"live", b"v".to_vec(), None, SetCondition::Always);
    expired_key(&mut s, b"gone", &[b'x'; 40]);
    let used = s.used_memory();
    assert_eq!(s.get(b"gone"), Ok(None));
    assert_eq!(s.expired_keys_total(), 1);
    assert_eq!(s.take_expired_keys(), vec![b"gone".to_vec()]);
    assert!(s.map.get(b"gone".as_slice()).is_none(), "removed, not just hidden");
    assert!(s.used_memory() < used, "its bytes left the account");
    assert_eq!(s.get(b"gone"), Ok(None));
    assert_eq!(s.expired_keys_total(), 1, "a miss counts nothing");
    assert_eq!(s.get(b"live").map(|v| v.map(|c| c.into_owned())), Ok(Some(b"v".to_vec())));
    assert_eq!(s.expired_keys_total(), 1);
}

#[test]
fn a_write_through_an_expired_key_starts_it_fresh() {
    let mut s = Store::new();
    expired_key(&mut s, b"n", b"41");
    assert_eq!(s.incr_by(b"n", 1), Ok(1), "the expired 41 is not the base");
    assert_eq!(s.expired_keys_total(), 1);
    assert_eq!(s.map.get(b"n".as_slice()).and_then(|e| e.expire_at_ns), None);
    s.set(b"t", b"1".to_vec(), Some(Duration::from_secs(100)), SetCondition::Always);
    assert_eq!(s.incr_by(b"t", 1), Ok(2));
    assert!(s.map.get(b"t".as_slice()).and_then(|e| e.expire_at_ns).is_some(), "TTL kept");
}

#[test]
fn a_hit_touches_the_access_clock_and_a_miss_does_not() {
    let mut s = Store::new();
    s.set_max_memory(1 << 30, EvictionPolicy::AllKeysLru);
    s.set(b"a", b"1".to_vec(), None, SetCondition::Always);
    s.set(b"b", b"1".to_vec(), None, SetCondition::Always);
    let tick = s.clock_counter;
    assert_eq!(s.get(b"missing"), Ok(None));
    assert_eq!(s.clock_counter, tick, "a miss does not advance the clock");
    assert!(s.get(b"a").is_ok());
    assert_eq!(s.clock_counter, tick + 1);
    let lru = |s: &Store, k: &[u8]| s.clock_of(k);
    assert_eq!(lru(&s, b"a"), Some((tick + 1) as u32), "the read stamped its entry");
    assert_eq!(s.incr_by(b"b", 1), Ok(2));
    assert_eq!(lru(&s, b"b"), Some((tick + 2) as u32), "so did the read-modify-write");
    assert_eq!(lru(&s, b"a"), Some((tick + 1) as u32), "and nothing else");
}

#[test]
fn only_the_mutable_read_records_the_row() {
    let mut s = Store::new();
    s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec()]));
    s.set(b"u:n", b"1".to_vec(), None, SetCondition::Always);
    s.take_row_changes(RowChanges::default());
    assert!(s.get(b"u:n").is_ok());
    let seen = s.take_row_changes(RowChanges::default());
    assert_eq!(seen.iter().count(), 0, "a read leaves the row alone");
    assert_eq!(s.incr_by(b"u:n", 1), Ok(2));
    let seen = s.take_row_changes(RowChanges::default());
    let keys: Vec<&[u8]> = seen.iter().map(|r| r.key()).collect();
    assert_eq!(keys, [b"u:n".as_slice()], "the write records its row once");
}
