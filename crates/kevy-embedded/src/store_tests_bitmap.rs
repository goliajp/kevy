//! Tests for the bitmap ops in `ops_bitmap.rs`
//! (kevy-embedded 1.8.0).

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- getbit on absent key ------------------------------------------------

#[test]
fn getbit_absent_returns_zero() {
    let s = s();
    assert_eq!(s.getbit(b"k", 0).unwrap(), 0);
    assert_eq!(s.getbit(b"k", 100).unwrap(), 0);
}

// ---- setbit + getbit round-trip ------------------------------------------

#[test]
fn setbit_at_offset_zero_msb_first() {
    let s = s();
    // bit 0 = MSB of byte 0. Setting it makes byte 0 = 0b10000000 = 0x80.
    let prev = s.setbit(b"k", 0, 1).unwrap();
    assert_eq!(prev, 0);
    assert_eq!(s.getbit(b"k", 0).unwrap(), 1);
    assert_eq!(s.get(b"k").unwrap(), Some(vec![0x80]));
}

#[test]
fn setbit_growing_extends_with_zero_padding() {
    let s = s();
    // Set bit 16 (= MSB of byte 2). String grows to 3 bytes.
    let prev = s.setbit(b"k", 16, 1).unwrap();
    assert_eq!(prev, 0);
    let bytes = s.get(b"k").unwrap().unwrap();
    assert_eq!(bytes.len(), 3);
    assert_eq!(bytes, vec![0x00, 0x00, 0x80]);
}

#[test]
fn setbit_returns_previous() {
    let s = s();
    s.setbit(b"k", 3, 1).unwrap();
    let prev = s.setbit(b"k", 3, 0).unwrap();
    assert_eq!(prev, 1);
    assert_eq!(s.getbit(b"k", 3).unwrap(), 0);
}

#[test]
fn setbit_invalid_value_errors() {
    let s = s();
    assert!(s.setbit(b"k", 0, 2).is_err());
}

// ---- bitcount ------------------------------------------------------------

#[test]
fn bitcount_empty_or_absent_is_zero() {
    let s = s();
    assert_eq!(s.bitcount(b"k", None).unwrap(), 0);
    s.set(b"k", b"").unwrap();
    assert_eq!(s.bitcount(b"k", None).unwrap(), 0);
}

#[test]
fn bitcount_full_string() {
    let s = s();
    // "abc" = 0x61 0x62 0x63 = 0b01100001 0b01100010 0b01100011.
    // set bits: 3 + 3 + 4 = 10.
    s.set(b"k", b"abc").unwrap();
    assert_eq!(s.bitcount(b"k", None).unwrap(), 10);
}

#[test]
fn bitcount_with_byte_range() {
    let s = s();
    s.set(b"k", b"abc").unwrap();
    // Byte 0 only ('a' = 3 set bits).
    assert_eq!(s.bitcount(b"k", Some((0, 0))).unwrap(), 3);
    // Bytes 1-2 ('b' + 'c' = 3 + 4 = 7).
    assert_eq!(s.bitcount(b"k", Some((1, 2))).unwrap(), 7);
}

#[test]
fn bitcount_negative_indexing() {
    let s = s();
    s.set(b"k", b"abc").unwrap();
    // Last byte = 'c' = 4 set bits.
    assert_eq!(s.bitcount(b"k", Some((-1, -1))).unwrap(), 4);
}
