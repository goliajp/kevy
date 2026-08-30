//! String ops × value encodings (`Value::Str` / `Value::Int` /
//! `Value::ArcBulk`): every string op must accept all three.
//!
//! Guards a compat divergence found in CI:
//! `SET x 5` stores `Value::Int` (integer encoding), but
//! GETDEL / GETSET / INCRBYFLOAT / APPEND still had
//! `Value::Str`-only arms and replied WRONGTYPE where Redis/valkey
//! succeed.

use crate::Store;

/// Larger than BULK_THRESHOLD so the value lands as `Value::ArcBulk`.
fn big(fill: u8) -> Vec<u8> {
    vec![fill; 9000]
}

fn set(s: &mut Store, k: &[u8], v: &[u8]) {
    s.set(k, v.to_vec(), None, false, false);
}

#[test]
fn getdel_accepts_all_string_encodings() {
    let mut s = Store::new();
    set(&mut s, b"i", b"5");
    assert_eq!(s.getdel(b"i").unwrap(), Some(b"5".to_vec()), "Int encoding");
    assert!(s.get(b"i").unwrap().is_none(), "Int-encoded key not deleted");

    set(&mut s, b"s", b"hello");
    assert_eq!(s.getdel(b"s").unwrap(), Some(b"hello".to_vec()), "Str encoding");

    let b = big(b'x');
    set(&mut s, b"b", &b);
    assert_eq!(s.getdel(b"b").unwrap(), Some(b.clone()), "ArcBulk encoding");
    assert!(s.get(b"b").unwrap().is_none());
}

#[test]
fn getset_accepts_all_string_encodings() {
    let mut s = Store::new();
    set(&mut s, b"i", b"42");
    assert_eq!(s.getset(b"i", b"new".to_vec()).unwrap(), Some(b"42".to_vec()), "Int encoding");
    assert_eq!(s.get(b"i").unwrap().unwrap().as_ref(), b"new");

    let b = big(b'y');
    set(&mut s, b"b", &b);
    assert_eq!(s.getset(b"b", b"tiny".to_vec()).unwrap(), Some(b), "ArcBulk");
}

#[test]
fn getset_stores_new_value_with_canonical_encoding() {
    let mut s = Store::new();
    set(&mut s, b"k", b"old");
    // New value is a canonical integer: follow-up INCR must take the
    // Int fast path (i.e. GETSET must route through the same encoding
    // rules as SET).
    s.getset(b"k", b"41".to_vec()).unwrap();
    assert_eq!(s.incr_by(b"k", 1).unwrap(), 42);
}

#[test]
fn incr_by_float_accepts_int_encoding() {
    let mut s = Store::new();
    set(&mut s, b"i", b"5");
    assert_eq!(s.incr_by_float(b"i", 1.5).unwrap(), b"6.5".to_vec());
    assert_eq!(s.get(b"i").unwrap().unwrap().as_ref(), b"6.5");
}

#[test]
fn append_accepts_int_encoding() {
    let mut s = Store::new();
    set(&mut s, b"i", b"12");
    assert_eq!(s.append(b"i", b"ab").unwrap(), 4);
    assert_eq!(s.get(b"i").unwrap().unwrap().as_ref(), b"12ab");
    // And the result re-parses as non-canonical → stays a plain string;
    // appending digits to make it canonical again must still INCR.
    set(&mut s, b"j", b"1");
    s.append(b"j", b"2").unwrap();
    assert_eq!(s.incr_by(b"j", 1).unwrap(), 13);
}

#[test]
fn wrongtype_still_rejected_for_real_collections() {
    let mut s = Store::new();
    s.rpush(b"l", &[b"a".as_slice()]).unwrap();
    assert!(s.getdel(b"l").is_err(), "GETDEL on a list must stay WRONGTYPE");
    assert!(s.getset(b"l", b"v".to_vec()).is_err());
    assert!(s.incr_by_float(b"l", 1.0).is_err());
    assert!(s.append(b"l", b"x").is_err());
}
