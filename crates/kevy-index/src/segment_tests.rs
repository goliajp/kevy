use super::*;
use crate::composite::{CompositeCol, composite_encode};
use crate::{IndexKind, ValType};
use kevy_text::SortOrder;

fn i(v: i64) -> IndexValue {
    IndexValue::I64(v)
}

fn seeded() -> Segment {
    let mut s = Segment::new();
    for (k, v) in [("u1", 30), ("u2", 25), ("u3", 30), ("u4", 40), ("u5", 18)] {
        s.apply(k.as_bytes(), None, Some(i(v)));
    }
    s
}

#[test]
fn apply_replace_remove_and_stats() {
    let mut s = seeded();
    assert_eq!(s.stats().entries, 5);
    assert_eq!(s.stats().duplicates, 1, "30 held twice");
    // replace u1's value: 30 no longer duplicated
    s.apply(b"u1", Some(&i(30)), Some(i(31)));
    assert_eq!(s.stats().entries, 5);
    assert_eq!(s.stats().duplicates, 0);
    // coerce-failure excludes and counts
    s.apply(b"u2", Some(&i(25)), None);
    assert_eq!(s.stats().entries, 4);
    assert_eq!(s.stats().coerce_failures, 1);
    // remove is not a coerce failure
    s.remove(b"u3", &i(30));
    assert_eq!(s.stats().entries, 3);
    assert_eq!(s.stats().coerce_failures, 1);
    assert!(!s.contains(&i(30), b"u3"));
    assert!(s.contains(&i(40), b"u4"));
}

#[test]
fn range_scan_orders_and_paginates() {
    let s = seeded();
    let (page1, cur) = s.range(&i(18), &i(30), None, 2);
    assert_eq!(page1[0], (b"u5".to_vec(), i(18)));
    assert_eq!(page1[1], (b"u2".to_vec(), i(25)));
    let cur = cur.expect("more pages");
    let (page2, cur2) = s.range(&i(18), &i(30), Some(&cur), 10);
    assert_eq!(
        page2,
        vec![(b"u1".to_vec(), i(30)), (b"u3".to_vec(), i(30))],
        "value tie broken by key"
    );
    assert!(cur2.is_none(), "exhausted");
    assert_eq!(s.count(&i(18), &i(30)), 4);
    assert_eq!(s.count(&i(99), &i(100)), 0);
    assert_eq!(s.count(&i(31), &i(30)), 0, "an empty interval");
}

#[test]
fn eq_and_duplicate_fence() {
    let s = seeded();
    assert_eq!(s.eq(&i(30), 10), vec![b"u1".to_vec(), b"u3".to_vec()]);
    assert_eq!(s.eq(&i(40), 10), vec![b"u4".to_vec()]);
    assert!(s.eq(&i(99), 10).is_empty());
}

#[test]
fn long_keys_at_max_value_not_missed() {
    let mut s = Segment::new();
    let long_key = vec![0xFFu8; 80];
    s.apply(&long_key, None, Some(i(30)));
    s.apply(b"short", None, Some(i(30)));
    let (hits, _) = s.range(&i(30), &i(30), None, 10);
    assert_eq!(hits.len(), 2, "max-valued long key must not be missed");
    assert_eq!(s.eq(&i(30), 10).len(), 2);
    assert_eq!(s.count(&i(30), &i(30)), 2);
}

#[test]
fn f64_and_str_orders() {
    let mut s = Segment::new();
    s.apply(b"a", None, Some(IndexValue::F64(1.5)));
    s.apply(b"b", None, Some(IndexValue::F64(-0.5)));
    let (hits, _) = s.range(&IndexValue::F64(-1.0), &IndexValue::F64(2.0), None, 10);
    assert_eq!(hits[0].0, b"b".to_vec());

    let mut t = Segment::new();
    t.apply(b"x", None, Some(IndexValue::Str(b"banana".to_vec())));
    t.apply(b"y", None, Some(IndexValue::Str(b"apple".to_vec())));
    let (hits, _) =
        t.range(&IndexValue::Str(b"a".to_vec()), &IndexValue::Str(b"z".to_vec()), None, 10);
    assert_eq!(hits[0].0, b"y".to_vec());
}

#[test]
fn a_value_of_another_type_is_excluded_and_bounds_of_another_type_order_by_type() {
    let mut s = Segment::new();
    s.apply(b"a", None, Some(i(1)));
    s.apply(b"b", None, Some(IndexValue::Str(b"x".to_vec())));
    assert_eq!((s.stats().entries, s.stats().coerce_failures), (1, 1));
    let low = IndexValue::Str(b"a".to_vec());
    // a str bound sorts above every i64, as IndexValue orders them
    assert_eq!(s.range(&i(0), &low, None, 10).0.len(), 1);
    assert!(s.range(&low, &low, None, 10).0.is_empty());
    assert_eq!(s.count(&i(i64::MIN), &IndexValue::F64(0.0)), 1);
    let past = Cursor::new(IndexValue::F64(0.0), Vec::new());
    assert!(s.range(&i(0), &i(9), Some(&past), 10).0.is_empty());
}

#[test]
fn a_key_that_breaks_the_digit_form_re_encodes_the_segment() {
    let spec = crate::IndexSpec::builder("n", "row:", IndexKind::Range, ValType::I64)
        .with_field("n")
        .build()
        .expect("a spec");
    let mut s = Segment::for_spec(&spec);
    for k in 0..500 {
        s.apply(format!("row:{k}").as_bytes(), None, Some(i(k % 7)));
    }
    assert!(s.codec.digits);
    s.apply(b"row:x", None, Some(i(3)));
    s.apply(b"other:1", None, Some(i(3)));
    assert!(!s.codec.digits && s.codec.prefix.is_empty());
    let mut all = Vec::new();
    s.each_entry(|k, v| all.push((v.clone(), k.to_vec())));
    let mut want: Vec<(IndexValue, Vec<u8>)> =
        (0..500).map(|k| (i(k % 7), format!("row:{k}").into_bytes())).collect();
    want.push((i(3), b"row:x".to_vec()));
    want.push((i(3), b"other:1".to_vec()));
    want.sort();
    assert_eq!(all, want);
    assert_eq!(s.stats().duplicates, 7);
}

#[test]
fn a_cursor_key_the_segment_cannot_encode_still_positions() {
    let spec = crate::IndexSpec::builder("n", "row:", IndexKind::Range, ValType::I64)
        .with_field("n")
        .build()
        .expect("a spec");
    let mut s = Segment::for_spec(&spec);
    for k in ["row:1", "row:2", "row:30"] {
        s.apply(k.as_bytes(), None, Some(i(5)));
    }
    // "row:2x" sorts between row:2 and row:30, and is not digits
    let c = Cursor::new(i(5), b"row:2x".to_vec());
    assert_eq!(s.range(&i(0), &i(9), Some(&c), 10).0, vec![(b"row:30".to_vec(), i(5))]);
    let mut back = s.scan(Some(&c), SortOrder::Desc);
    assert_eq!(back.next_entry().map(|(_, k)| k.to_vec()), Some(b"row:2".to_vec()));
}

#[test]
fn a_composite_spec_stores_the_encoding_as_it_is() {
    let cols = vec![
        CompositeCol::new("s", ValType::Str),
        CompositeCol::new("t", ValType::I64).with_order(SortOrder::Desc),
    ];
    let spec = crate::IndexSpec::builder("p", "t:", IndexKind::Range, ValType::Str)
        .with_field("p")
        .with_composite(cols.clone())
        .build()
        .expect("a spec");
    let mut s = Segment::for_spec(&spec);
    let enc = |st: &str, t: &str| {
        IndexValue::Str(
            composite_encode(&cols, &[Some(st.as_bytes()), Some(t.as_bytes())]).expect("coerces"),
        )
    };
    s.apply(b"t:1", None, Some(enc("a", "5")));
    s.apply(b"t:2", None, Some(enc("a", "9")));
    s.apply(b"t:3", None, Some(enc("b", "1")));
    let (hits, _) = s.range(&enc("a", "99"), &enc("a", "0"), None, 10);
    let keys: Vec<_> = hits.iter().map(|h| h.0.clone()).collect();
    assert_eq!(keys, vec![b"t:2".to_vec(), b"t:1".to_vec()], "status a, newest first");
    assert_eq!(s.max_value(), Some(enc("b", "1")));
}

#[test]
fn window_cuts_keep_a_deep_tree_whole() {
    let mut s = Segment::with_values(2);
    for k in 0..60_000i64 {
        let key = format!("row:{}", k * 7919 % 60_000);
        s.apply_with_values(key.as_bytes(), None, Some(i(k % 997)), &[Some(b"c1"), Some(b"12")]);
    }
    let _ = crate::seg_tree::tests::check(&s.tree);
    for b in [5, 100, 400, 996] {
        s.split_off_below(&i(b));
        let got = crate::seg_tree::tests::check(&s.tree);
        assert_eq!(got.len(), s.stats().entries as usize);
        assert_eq!(s.range(&i(0), &i(2000), None, 1).0.len(), 1, "after cut {b}");
    }
}

#[test]
fn a_repacked_build_keeps_every_row() {
    let spec = crate::IndexSpec::builder("n", "user:", IndexKind::Range, ValType::I64)
        .with_field("n")
        .build()
        .expect("a spec");
    let mut s = Segment::for_spec(&spec);
    let mut order: Vec<u32> = (0..8000).collect();
    let mut x = 7u64;
    for i in (1..order.len()).rev() {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        order.swap(i, (x >> 33) as usize % (i + 1));
    }
    for i in order {
        s.apply(
            format!("user:{i}").as_bytes(),
            None,
            Some(IndexValue::I64(i64::from(i * 7919 % 1000))),
        );
    }
    assert_eq!(s.range(&i(0), &i(1000), None, usize::MAX).0.len(), 8000);
    s.repack();
    let _ = crate::seg_tree::tests::check(&s.tree);
    assert_eq!(s.range(&i(0), &i(1000), None, usize::MAX).0.len(), 8000);
    assert_eq!(s.count(&i(0), &i(1000)), 8000);
    for n in [100u32, 500, 1999, 2000, 2001, 3000] {
        for seed in 0..20u64 {
            let mut s = Segment::for_spec(&spec);
            let mut x = seed;
            let mut keys: Vec<u32> = (0..n).collect();
            for i in (1..keys.len()).rev() {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                keys.swap(i, (x >> 33) as usize % (i + 1));
            }
            for k in &keys {
                s.apply(
                    format!("user:{}", k * 4).as_bytes(),
                    None,
                    Some(i(i64::from(k * 7919 % 1000))),
                );
            }
            s.repack();
            let _ = crate::seg_tree::tests::check(&s.tree);
            assert_eq!(
                s.range(&i(0), &i(1000), None, 10_000).0.len(),
                n as usize,
                "n={n} seed={seed}"
            );
        }
    }
}

#[test]
fn stats_count_what_the_structures_hold() {
    let mut s = Segment::with_values(1);
    let empty = s.stats().approx_bytes;
    for k in 0..10_000 {
        s.apply_with_values(format!("r{k}").as_bytes(), None, Some(i(k)), &[Some(b"v")]);
    }
    let full = s.stats().approx_bytes;
    let leaves = s.tree.live_leaves() as u64 * crate::seg_leaf::LEAF_BYTES as u64;
    assert!(full >= leaves && full > empty);
    assert!(full < leaves + 64 * 1024, "leaves dominate: {full} vs {leaves}");
}
