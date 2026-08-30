//! The cold-key codec's order fidelity, the bloom's no-false-negative
//! property, and the eviction cut's bookkeeping identity.

use super::*;
use crate::segment::Segment;
use crate::value::order_key;

fn v(i: i64) -> IndexValue {
    IndexValue::I64(i)
}

#[test]
fn value_order_bytes_pins_to_order_key() {
    for (ty, raws) in [
        (ValType::I64, vec![b"-5".to_vec(), b"0".to_vec(), b"42".to_vec()]),
        (ValType::F64, vec![b"-1.5".to_vec(), b"0".to_vec(), b"3.25".to_vec()]),
        (ValType::Str, vec![b"".to_vec(), b"abc".to_vec(), b"z\x00q".to_vec()]),
    ] {
        for raw in raws {
            let coerced = IndexValue::coerce(ty, &raw).expect("coerces");
            assert_eq!(
                value_order_bytes(&coerced),
                order_key(ty, &raw).expect("orders"),
                "{ty:?} {raw:?}"
            );
        }
    }
}

#[test]
fn seg_keys_sort_exactly_like_tree_entries() {
    // Values and keys chosen to stress the framing: embedded NULs,
    // prefixes, empty keys, negative numbers.
    let entries: Vec<(IndexValue, Vec<u8>)> = vec![
        (v(-10), b"row:9".to_vec()),
        (v(-10), b"row:10".to_vec()),
        (v(0), Vec::new()),
        (v(0), b"\x00".to_vec()),
        (v(0), b"\x00\x00".to_vec()),
        (v(0), b"a".to_vec()),
        (v(7), b"a".to_vec()),
        (IndexValue::Str(b"a".to_vec()), b"r1".to_vec()),
        (IndexValue::Str(b"a\x00".to_vec()), b"r1".to_vec()),
        (IndexValue::Str(b"a\x00b".to_vec()), b"r1".to_vec()),
        (IndexValue::Str(b"ab".to_vec()), b"r1".to_vec()),
    ];
    let mut tree_sorted = entries.clone();
    tree_sorted.sort();
    let mut byte_sorted = entries.clone();
    byte_sorted.sort_by_key(|(val, k)| seg_key(val, k));
    // Str and I64 never meet in one real index; compare within groups.
    let split = |es: &[(IndexValue, Vec<u8>)]| {
        es.iter().cloned().partition::<Vec<_>, _>(|(val, _)| matches!(val, IndexValue::I64(_)))
    };
    assert_eq!(split(&tree_sorted), split(&byte_sorted));
}

#[test]
fn seg_key_round_trips() {
    for (ty, val, row) in [
        (ValType::I64, v(-42), b"row:\x001".to_vec()),
        (ValType::I64, v(i64::MAX), Vec::new()),
        (ValType::F64, IndexValue::F64(-2.5), b"r".to_vec()),
        (ValType::Str, IndexValue::Str(b"x\x00y".to_vec()), b"\x00\x00".to_vec()),
    ] {
        let k = seg_key(&val, &row);
        let (dv, drow) = decode_seg_key(ty, &k).expect("decodes");
        assert_eq!((dv, drow), (val, row), "{ty:?}");
    }
    assert_eq!(decode_seg_key(ValType::I64, b"garbage"), None);
    assert_eq!(decode_seg_key(ValType::I64, &[]), None);
}

#[test]
fn bloom_never_forgets_and_rarely_lies() {
    let mut b = ColdBloom::new(1000);
    let item = |i: u32| format!("row:{i}").into_bytes();
    for i in 0..1000 {
        b.insert(&item(i));
    }
    for i in 0..1000 {
        assert!(b.contains(&item(i)), "false negative at {i}");
    }
    let fp = (10_000..20_000).filter(|&i| b.contains(&item(i))).count();
    assert!(fp < 500, "false-positive rate implausibly high: {fp}/10000");
}

#[test]
fn split_off_below_cuts_strictly_and_balances_the_books() {
    let mut s = Segment::new();
    for i in 0..100i64 {
        s.apply(format!("row:{i:03}").as_bytes(), Some(v(i)));
    }
    // Two rows share the boundary value: both must stay hot.
    s.apply(b"row:dup", Some(v(50)));
    let before = s.stats();

    let evicted = s.split_off_below(&v(50));
    assert_eq!(evicted.len(), 50, "strictly below the bound");
    assert!(evicted.iter().all(|(val, _)| *val < v(50)));
    assert!(evicted.windows(2).all(|w| w[0] < w[1]), "tree order");

    let after = s.stats();
    assert_eq!(after.entries, before.entries - 50);
    assert!(after.approx_bytes < before.approx_bytes);
    // The boundary-value rows survived, and the segment still serves.
    assert_eq!(s.count(&v(50), &v(50)), 2);
    assert_eq!(s.count(&v(0), &v(49)), 0);
    assert_eq!(s.count(&v(0), &v(999)), 51);

    // Evicted rows are fully forgotten: re-applying one is an insert,
    // not a replace (the reverse map was drained too).
    s.apply(b"row:007", Some(v(7)));
    assert_eq!(s.stats().entries, after.entries + 1);

    // Emptying cut: everything strictly below MAX goes, books hit zero.
    let rest = s.split_off_below(&IndexValue::I64(i64::MAX));
    assert_eq!(rest.len(), 52);
    assert_eq!(s.stats().entries, 0);
    assert_eq!(s.count(&v(0), &v(999)), 0);
}

#[test]
fn seg_bounds_cover_exactly_the_value_interval() {
    // Keys for values -1..=5 with assorted row keys, including a row
    // key starting 0xFF (the case a naive suffix bound misses).
    let mut keys = Vec::new();
    for i in -1..=5i64 {
        for rk in [b"".to_vec(), b"\xffz".to_vec(), b"row:1".to_vec(), b"\x00".to_vec()] {
            keys.push((i, rk.clone(), seg_key(&v(i), &rk)));
        }
    }
    let (lo, hi) = seg_bounds(&v(0), &v(3));
    let hits: Vec<i64> = keys
        .iter()
        .filter(|(_, _, k)| k.as_slice() >= lo.as_slice() && k.as_slice() <= hi.as_slice())
        .map(|(i, _, _)| *i)
        .collect();
    assert_eq!(hits.len(), 16, "4 values x 4 row keys: {hits:?}");
    assert!(hits.iter().all(|i| (0..=3).contains(i)));
}

#[test]
fn seg_values_payload_round_trips_and_refuses_garbage() {
    let cases: &[&[Option<&[u8]>]] = &[
        &[],
        &[None],
        &[Some(b"42"), None, Some(b"")],
        &[Some(b"a-longer-value-with-bytes\x00inside"), Some(b"x")],
    ];
    for vals in cases {
        let payload = encode_seg_values(vals);
        let back = decode_seg_values(&payload).expect("decodes");
        let want: Vec<Option<Vec<u8>>> = vals.iter().map(|o| o.map(<[u8]>::to_vec)).collect();
        assert_eq!(back, want);
    }
    // The empty payload IS the no-values shape (a-train segments).
    assert!(encode_seg_values(&[]).is_empty());
    // Truncation and a bad tag both refuse.
    let p = encode_seg_values(&[Some(b"hello"), None]);
    assert!(decode_seg_values(&p[..p.len() - 1]).is_none(), "truncated");
    assert!(decode_seg_values(&[9, 0, 0, 0]).is_none(), "bad shape");
}
