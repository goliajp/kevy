//! SETBIT, SETRANGE and BITFIELD write inside a bulk value in place only
//! while nothing else holds it: a pinned snapshot keeps what it pinned.

use alloc::vec::Vec;

use crate::{BitFieldOp, BitType, Overflow, SetCondition, Store, Value};

/// The bytes at `key` and where they live.
fn bulk(s: &Store, key: &[u8]) -> (Vec<u8>, *const u8) {
    let Some(Value::ArcBulk(a)) = s.map.get(key).map(|e| &e.value) else { panic!("a bulk value") };
    (a.to_vec(), a.as_ptr())
}

fn writes(s: &mut Store) {
    s.setbit(b"k", 9, 1).unwrap();
    s.setrange(b"k", 100, b"xyz").unwrap();
    let u8t = BitType::parse(b"u8").unwrap();
    s.bitfield(b"k", &[BitFieldOp::IncrBy(u8t, 16, 7, Overflow::Wrap)]).unwrap();
}

#[test]
fn writes_inside_an_unshared_value_happen_in_place() {
    let mut s = Store::new();
    s.set(b"k", alloc::vec![b'a'; 200], None, SetCondition::Always);
    s.setbit(b"k", 0, 0).unwrap();
    let (_, at) = bulk(&s, b"k");
    writes(&mut s);
    let (now, moved) = bulk(&s, b"k");
    assert_eq!(moved, at, "no copy for a write inside the value");
    assert_eq!(now.len(), 200);
    assert_eq!((now[1], &now[100..103], now[2]), (b'a' | 0x40, &b"xyz"[..], b'a' + 7));
}

#[test]
fn a_pinned_snapshot_keeps_its_bytes() {
    let mut s = Store::new();
    s.set(b"k", alloc::vec![b'a'; 200], None, SetCondition::Always);
    s.setbit(b"k", 0, 0).unwrap();
    let (before, _) = bulk(&s, b"k");
    let view = s.collect_snapshot();
    writes(&mut s);
    let mut pinned = Vec::new();
    view.each(|k, v, _| {
        if let (b"k", Value::ArcBulk(a)) = (k, v) {
            pinned = a.to_vec();
        }
    });
    assert_eq!(pinned, before, "the snapshot is untouched");
    assert_ne!(bulk(&s, b"k").0, before, "the live value took the writes");
}
