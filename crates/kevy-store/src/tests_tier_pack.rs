//! A cold row that fits a declared table comes back packed.
//!
//! A table declared over a keyspace that is already partly cold leaves its
//! cold rows cold: they hold no memory for the packed form to save. They
//! were demoted as general hashes, so without this they came back as
//! general hashes too, and every row that was cold at declaration time kept
//! the general form's cost for good once it was read again. Promotion
//! already reads and decodes the row; building the packed form there costs
//! less than the general hash it replaces.

#![allow(clippy::unwrap_used, clippy::panic)]

use crate::Store;
use crate::packed_row::ColumnNames;
use crate::value::Value;

fn tiered(name: &str) -> (Store, kevy_tmpdir::TmpDir) {
    let d = kevy_tmpdir::TmpDir::new(name);
    let mut s = Store::new();
    s.enable_tiering(d.path(), u64::MAX).unwrap();
    (s, d)
}

fn table() -> ColumnNames {
    vec![b"id".to_vec(), b"name".to_vec(), b"pad".to_vec()].into()
}

fn pad() -> Vec<u8> {
    (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
}

/// `key` written as a general hash and demoted before its table exists.
fn cold_row(s: &mut Store, key: &[u8], pairs: &[(&[u8], &[u8])]) {
    s.hset(key, pairs).unwrap();
    assert!(!s.is_packed(key));
    assert!(s.debug_force_demote(key));
}

/// A table's backfill reaching `key`: the row is cold, so it stays cold,
/// and the table's shape is kept for when it is read.
fn declare_over(s: &mut Store, key: &[u8], names: &ColumnNames) {
    s.set_packed_rows(true);
    s.pack_row(key, names);
    assert!(matches!(s.map.get(key).map(|e| &e.value), Some(Value::Cold(_))), "left cold");
}

#[test]
fn a_promoted_row_that_fits_its_table_comes_back_packed() {
    let (mut s, _d) = tiered("tier-pack-on-promote");
    let pad = pad();
    let pairs: [(&[u8], &[u8]); 3] = [(b"id", b"7"), (b"name", b"alice"), (b"pad", &pad)];
    cold_row(&mut s, b"row:1", &pairs);
    let names = table();
    declare_over(&mut s, b"row:1", &names);

    assert!(s.promote_in_place(b"row:1"));
    assert!(s.is_packed(b"row:1"), "a promoted row that fits its table takes the packed form");
    let Some(Value::PackedRow(r)) = s.map.get(b"row:1".as_slice()).map(|e| &e.value) else {
        unreachable!("checked packed above")
    };
    assert!(alloc::sync::Arc::ptr_eq(r.names(), &names), "on the table's own names");
    assert_eq!(s.hget(b"row:1", b"name").unwrap(), Some(b"alice".as_slice()));
    assert_eq!(s.hlen(b"row:1").unwrap(), 3);

    // charged exactly as the same row packed while hot
    let (mut hot, _d2) = tiered("tier-pack-on-promote-twin");
    hot.hset(b"row:1", &pairs).unwrap();
    hot.set_packed_rows(true);
    hot.pack_row(b"row:1", &names);
    let weight = |s: &Store| s.map.get(b"row:1".as_slice()).unwrap().weight();
    assert_eq!(weight(&s), weight(&hot), "the promoted row's charge is the packed row's");
    assert_eq!(s.used_memory(), hot.used_memory(), "and so is the store's");
}

#[test]
fn a_promoted_row_the_table_cannot_hold_stays_a_general_hash() {
    let (mut s, _d) = tiered("tier-pack-on-promote-refused");
    let pad = pad();
    // `note` is not a column of the table: packing would lose it
    let pairs: [(&[u8], &[u8]); 3] = [(b"id", b"7"), (b"note", b"x"), (b"pad", &pad)];
    cold_row(&mut s, b"row:1", &pairs);
    declare_over(&mut s, b"row:1", &table());

    assert!(s.promote_in_place(b"row:1"));
    assert!(!s.is_packed(b"row:1"));
    assert_eq!(s.hget(b"row:1", b"note").unwrap(), Some(b"x".as_slice()));
    assert_eq!(s.hlen(b"row:1").unwrap(), 3);
}

#[test]
fn with_packed_rows_off_a_promoted_row_stays_a_general_hash() {
    let (mut s, _d) = tiered("tier-pack-on-promote-off");
    let pad = pad();
    let pairs: [(&[u8], &[u8]); 2] = [(b"id", b"7"), (b"pad", &pad)];
    cold_row(&mut s, b"row:1", &pairs);
    declare_over(&mut s, b"row:1", &table());
    s.set_packed_rows(false);

    assert!(s.promote_in_place(b"row:1"));
    assert!(!s.is_packed(b"row:1"));
    assert_eq!(s.hlen(b"row:1").unwrap(), 2);
}

#[test]
fn with_field_deadlines_set_a_cold_row_keeps_its_table_for_when_it_is_read() {
    let (mut s, _d) = tiered("tier-pack-with-field-deadlines");
    let pad = pad();
    let pairs: [(&[u8], &[u8]); 2] = [(b"id", b"7"), (b"pad", &pad)];
    cold_row(&mut s, b"row:1", &pairs);
    // another row's field deadline puts every pack on the purging path
    s.hset(b"other", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let far = u64::try_from(now.as_millis()).unwrap() + 3_600_000;
    s.hexpire_at(b"other", &[b"f".as_slice()], far, crate::HExpireCond::Always).unwrap();
    let names = table();
    declare_over(&mut s, b"row:1", &names);

    assert!(s.promote_in_place(b"row:1"));
    assert!(s.is_packed(b"row:1"), "packed on the table kept while it was cold");
    assert_eq!(s.hget(b"row:1", b"id").unwrap(), Some(b"7".as_slice()));
}
