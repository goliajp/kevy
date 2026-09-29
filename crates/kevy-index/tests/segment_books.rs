//! The segment's books: the duplicate counter, lookups, and the window
//! cut leaving everything as if each entry had been removed.

use kevy_index::{IndexValue, Segment};

fn i(v: i64) -> IndexValue {
    IndexValue::I64(v)
}

#[test]
fn duplicates_follow_one_value_up_and_down() {
    let mut s = Segment::new();
    let dups = |s: &Segment| s.stats().duplicates;
    assert_eq!(dups(&s), 0, "no holders");
    s.apply(b"a", None, Some(i(7)));
    assert_eq!(dups(&s), 0, "one holder");
    s.apply(b"b", None, Some(i(7)));
    assert_eq!(dups(&s), 1, "two holders");
    s.apply(b"c", None, Some(i(7)));
    assert_eq!(dups(&s), 1, "three holders");
    s.remove(b"c", &i(7));
    assert_eq!(dups(&s), 1, "back to two");
    s.apply(b"b", Some(&i(7)), Some(i(8)));
    assert_eq!(dups(&s), 0, "b moved away: one holder");
    s.apply(b"a", Some(&i(7)), None);
    assert_eq!(dups(&s), 0, "none left");
    assert_eq!(s.eq(&i(7), 10), Vec::<Vec<u8>>::new());
}

#[test]
fn duplicates_are_counted_per_value_not_per_extra_holder() {
    let mut s = Segment::new();
    for k in [b"a", b"b", b"c", b"d"] {
        s.apply(k, None, Some(i(1)));
    }
    s.apply(b"e", None, Some(i(2)));
    s.apply(b"f", None, Some(i(2)));
    assert_eq!(s.stats().duplicates, 2);
    s.apply(b"a", Some(&i(1)), Some(i(1)));
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
        s.apply_with_values(k, None, Some(i(v)), &[Some(k)]);
    }
    s.apply(b"z", None, None);
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
    for (v, k) in &evicted {
        one_by_one.remove(k, v);
    }
    assert_eq!(cut.stats(), one_by_one.stats());
    assert_eq!(cut.stats().duplicates, 1, "only 10 is still held twice");
    for (v, k) in &evicted {
        assert!(!cut.contains(v, k), "a cut entry is gone");
        assert_eq!(cut.stored(v, k, 0), None, "and its stored values");
    }
    let mut left = Vec::new();
    cut.each_entry(|k, v| left.push((k.to_vec(), v.clone())));
    left.sort();
    assert_eq!(left, vec![(b"f".to_vec(), i(10)), (b"g".to_vec(), i(10)), (b"h".to_vec(), i(12))]);
    assert_eq!(cut.max_value(), Some(i(12)));
}

#[test]
fn lookups_by_value_and_key() {
    let mut s = Segment::new();
    let owned: Vec<u8> = b"user:1".to_vec();
    s.apply(&owned, None, Some(i(1)));
    assert!(s.contains(&i(1), owned.as_slice()));
    assert!(!s.contains(&i(1), b"user:10"));

    s.apply(b"user:1", Some(&i(1)), Some(i(2)));
    assert!(s.contains(&i(2), b"user:1"), "the later value wins");
    assert!(s.eq(&i(1), 10).is_empty(), "the earlier value is gone");
    assert_eq!(s.stats().entries, 1);

    s.apply(b"", None, Some(i(3)));
    assert!(s.contains(&i(3), b""), "the empty key is a key");
    assert_eq!(s.eq(&i(3), 10), vec![Vec::<u8>::new()]);

    let long = vec![0xFFu8; 80];
    s.apply(&long, None, Some(i(3)));
    assert!(s.contains(&i(3), &long));
    assert_eq!(s.count(&i(3), &i(3)), 2);
    s.remove(&long, &i(3));
    assert!(!s.contains(&i(3), &long));
    assert_eq!(s.stats().entries, 2);
}

#[test]
fn a_stale_old_value_removes_nothing() {
    // a row the build has not reached yet: its write names an old value
    // the segment never held, and must not disturb anything else
    let mut s = Segment::new();
    s.apply(b"a", None, Some(i(1)));
    s.apply(b"b", Some(&i(1)), Some(i(2)));
    assert!(s.contains(&i(1), b"a") && s.contains(&i(2), b"b"));
    s.remove(b"c", &i(9));
    assert_eq!(s.stats().entries, 2);
}

#[test]
fn a_segment_can_cross_threads() {
    fn send_sync<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    send_sync::<Segment>();
}

/// The same operations report the same size in every segment.
#[test]
fn the_reported_size_does_not_depend_on_the_hash_seed() {
    let build = || {
        let mut s = windowed();
        for (k, v) in [(&b"a"[..], 5), (b"c", 5), (b"e", 9), (b"g", 10)] {
            s.remove(k, &i(v));
        }
        s.stats()
    };
    let first = build();
    for _ in 0..64 {
        assert_eq!(build(), first);
    }
}
