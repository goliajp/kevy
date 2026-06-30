//! Tests for the bitmap ops in `ops_bitmap.rs`
//! (kevy-embedded 1.8.0).

use crate::Config;
use crate::ops_bitmap::BitOp;
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

// ---- bitpos (kevy-embedded 1.14.0) -------------------------------------

#[test]
fn bitpos_first_set_bit_msb_first() {
    let s = s();
    // byte 0 = 0x40 = 0b01000000; first 1 = bit index 1.
    s.set(b"k", &[0x40]).unwrap();
    assert_eq!(s.bitpos(b"k", 1, None).unwrap(), Some(1));
}

#[test]
fn bitpos_first_zero_bit() {
    let s = s();
    // byte 0 = 0xff (all set); byte 1 = 0xfe = 0b11111110; first 0 = bit 15.
    s.set(b"k", &[0xff, 0xfe]).unwrap();
    assert_eq!(s.bitpos(b"k", 0, None).unwrap(), Some(15));
}

#[test]
fn bitpos_not_found_returns_none() {
    let s = s();
    // All set; searching for 0 in a defined range returns None.
    s.set(b"k", &[0xff, 0xff]).unwrap();
    assert_eq!(s.bitpos(b"k", 0, Some((0, 1))).unwrap(), None);
}

#[test]
fn bitpos_absent_key_returns_zero_for_clear_bit() {
    let s = s();
    // Searching for bit=0 in absent key returns Some(0) per Redis semantics.
    assert_eq!(s.bitpos(b"absent", 0, None).unwrap(), Some(0));
    // Searching for bit=1 in absent key returns None.
    assert_eq!(s.bitpos(b"absent", 1, None).unwrap(), None);
}

// ---- getrange / setrange (kevy-embedded 1.14.0) -----------------------

#[test]
fn getrange_basic_slice() {
    let s = s();
    s.set(b"k", b"Hello, World!").unwrap();
    assert_eq!(s.getrange(b"k", 0, 4).unwrap(), b"Hello".to_vec());
    assert_eq!(s.getrange(b"k", 7, 11).unwrap(), b"World".to_vec());
}

#[test]
fn getrange_negative_indexing() {
    let s = s();
    s.set(b"k", b"abcde").unwrap();
    // -3..=-1 = "cde"
    assert_eq!(s.getrange(b"k", -3, -1).unwrap(), b"cde".to_vec());
}

#[test]
fn getrange_absent_returns_empty() {
    let s = s();
    assert_eq!(s.getrange(b"absent", 0, 10).unwrap(), Vec::<u8>::new());
}

#[test]
fn setrange_in_bounds_overwrites() {
    let s = s();
    s.set(b"k", b"Hello, World!").unwrap();
    let n = s.setrange(b"k", 7, b"Redis").unwrap();
    assert_eq!(n, 13);
    assert_eq!(s.get(b"k").unwrap(), Some(b"Hello, Redis!".to_vec()));
}

#[test]
fn setrange_past_end_extends_with_zeros() {
    let s = s();
    let n = s.setrange(b"k", 5, b"World").unwrap();
    assert_eq!(n, 10);
    assert_eq!(
        s.get(b"k").unwrap(),
        Some(vec![0, 0, 0, 0, 0, b'W', b'o', b'r', b'l', b'd'])
    );
}

// ---- BITOP --------------------------------------------------------------

#[test]
fn bitop_and_intersection() {
    let s = s();
    s.set(b"a", &[0xf0, 0x0f]).unwrap();
    s.set(b"b", &[0xff, 0xff]).unwrap();
    let n = s.bitop(BitOp::And, b"d", &[b"a", b"b"]).unwrap();
    assert_eq!(n, 2);
    assert_eq!(s.get(b"d").unwrap(), Some(vec![0xf0, 0x0f]));
}

#[test]
fn bitop_or_union() {
    let s = s();
    s.set(b"a", &[0xf0, 0x00]).unwrap();
    s.set(b"b", &[0x0f, 0xff]).unwrap();
    let n = s.bitop(BitOp::Or, b"d", &[b"a", b"b"]).unwrap();
    assert_eq!(n, 2);
    assert_eq!(s.get(b"d").unwrap(), Some(vec![0xff, 0xff]));
}

#[test]
fn bitop_xor_diff() {
    let s = s();
    s.set(b"a", &[0xff]).unwrap();
    s.set(b"b", &[0x0f]).unwrap();
    let n = s.bitop(BitOp::Xor, b"d", &[b"a", b"b"]).unwrap();
    assert_eq!(n, 1);
    assert_eq!(s.get(b"d").unwrap(), Some(vec![0xf0]));
}

#[test]
fn bitop_not_one_source() {
    let s = s();
    s.set(b"a", &[0x0f]).unwrap();
    let n = s.bitop(BitOp::Not, b"d", &[b"a"]).unwrap();
    assert_eq!(n, 1);
    assert_eq!(s.get(b"d").unwrap(), Some(vec![0xf0]));
}

#[test]
fn bitop_not_rejects_multiple_sources() {
    let s = s();
    s.set(b"a", &[0x00]).unwrap();
    s.set(b"b", &[0x00]).unwrap();
    assert!(s.bitop(BitOp::Not, b"d", &[b"a", b"b"]).is_err());
}

#[test]
fn bitop_extends_shorter_sources_with_zeros() {
    let s = s();
    s.set(b"a", &[0xff, 0xff, 0xff]).unwrap();
    s.set(b"b", &[0x0f]).unwrap();
    // OR with a 1-byte src + a 3-byte src = 3-byte result.
    let n = s.bitop(BitOp::Or, b"d", &[b"a", b"b"]).unwrap();
    assert_eq!(n, 3);
    assert_eq!(s.get(b"d").unwrap(), Some(vec![0xff, 0xff, 0xff]));
}

// ---- TIME ---------------------------------------------------------------

#[test]
fn time_returns_unix_seconds_and_micros() {
    let s = s();
    let (secs, micros) = s.time();
    assert!(secs > 1_700_000_000, "expected post-2023 timestamp, got {secs}");
    assert!(micros < 1_000_000, "micros must be < 1s, got {micros}");
}
