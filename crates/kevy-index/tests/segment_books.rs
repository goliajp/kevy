//! The segment's books: the duplicate counter, lookups by key, and the
//! window cut leaving everything as if each entry had been removed.

use kevy_index::{IndexValue, Segment};

fn i(v: i64) -> IndexValue {
    IndexValue::I64(v)
}

#[test]
fn duplicates_follow_one_value_up_and_down() {
    let mut s = Segment::new();
    let dups = |s: &Segment| s.stats().duplicates;
    assert_eq!(dups(&s), 0, "no holders");
    s.apply(b"a", Some(i(7)));
    assert_eq!(dups(&s), 0, "one holder");
    s.apply(b"b", Some(i(7)));
    assert_eq!(dups(&s), 1, "two holders");
    s.apply(b"c", Some(i(7)));
    assert_eq!(dups(&s), 1, "three holders");
    s.remove(b"c");
    assert_eq!(dups(&s), 1, "back to two");
    s.apply(b"b", Some(i(8)));
    assert_eq!(dups(&s), 0, "b moved away: one holder");
    s.apply(b"a", None);
    assert_eq!(dups(&s), 0, "none left");
    assert_eq!(s.eq(&i(7), 10), Vec::<Vec<u8>>::new());
}

#[test]
fn duplicates_are_counted_per_value_not_per_extra_holder() {
    let mut s = Segment::new();
    for k in [b"a", b"b", b"c", b"d"] {
        s.apply(k, Some(i(1)));
    }
    s.apply(b"e", Some(i(2)));
    s.apply(b"f", Some(i(2)));
    assert_eq!(s.stats().duplicates, 2);
    s.apply(b"a", Some(i(1)));
    assert_eq!(s.stats().duplicates, 2, "re-applying the held value changes nothing");
}

fn windowed() -> Segment {
    let mut s = Segment::with_values(1);
    let rows: [(&[u8], i64); 9] = [
        (b"a", 5),
        (b"b", 5),
        (b"c", 5),
        (b"d", 9),
        (b"e", 9),
        (b"f", 10),
        (b"g", 10),
        (b"h", 12),
        (b"i", 3),
    ];
    for (k, v) in rows {
        s.apply_with_values(k, Some(i(v)), &[Some(k)]);
    }
    s.apply(b"z", None);
    s
}

#[test]
fn the_window_cut_equals_removing_each_entry() {
    let mut cut = windowed();
    assert_eq!(cut.stats().duplicates, 3);
    let evicted = cut.split_off_below(&i(10));
    let keys: Vec<&[u8]> = evicted.iter().map(|(_, k)| k.as_slice()).collect();
    assert_eq!(keys, [&b"i"[..], b"a", b"b", b"c", b"d", b"e"], "tree order, strictly below");

    let mut one_by_one = windowed();
    for k in &keys {
        one_by_one.remove(k);
    }
    assert_eq!(cut.stats(), one_by_one.stats());
    assert_eq!(cut.stats().duplicates, 1, "only 10 is still held twice");
    for k in &keys {
        assert_eq!(cut.verify_entry(k), None, "a cut key must leave the reverse side");
        assert_eq!(cut.stored(k, 0), None, "and its stored values");
    }
    let mut left = Vec::new();
    cut.each_entry(|k, v| left.push((k.to_vec(), v.clone())));
    left.sort();
    assert_eq!(left, vec![(b"f".to_vec(), i(10)), (b"g".to_vec(), i(10)), (b"h".to_vec(), i(12))]);
    assert_eq!(cut.max_value(), Some(&i(12)));
}

#[test]
fn lookups_by_borrowed_key() {
    let mut s = Segment::new();
    let owned: Vec<u8> = b"user:1".to_vec();
    s.apply(&owned, Some(i(1)));
    assert_eq!(s.verify_entry(owned.as_slice()), Some(&i(1)));
    assert_eq!(s.verify_entry(b"user:10"), None);

    s.apply(b"user:1", Some(i(2)));
    assert_eq!(s.verify_entry(b"user:1"), Some(&i(2)), "the later value wins");
    assert!(s.eq(&i(1), 10).is_empty(), "the earlier value is gone");
    assert_eq!(s.stats().entries, 1);

    s.apply(b"", Some(i(3)));
    assert_eq!(s.verify_entry(b""), Some(&i(3)), "the empty key is a key");
    assert_eq!(s.eq(&i(3), 10), vec![Vec::<u8>::new()]);

    let long = vec![0xFFu8; 80];
    s.apply(&long, Some(i(3)));
    assert_eq!(s.verify_entry(&long), Some(&i(3)));
    assert_eq!(s.count(&i(3), &i(3)), 2);
    s.remove(&long);
    assert_eq!(s.verify_entry(&long), None);
    assert_eq!(s.stats().entries, 2);
}

#[test]
fn a_segment_can_cross_threads() {
    fn send_sync<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    send_sync::<Segment>();
}
