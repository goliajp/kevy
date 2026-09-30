//! Reads of a cold value that leave it cold: a snapshot's pinned segment,
//! a row peek, and the lent read of a demoted string.

use crate::{SetCondition, Store, Value};

/// A store whose `user:1` row lives in a sealed row segment.
fn seg_backed(d: &kevy_tmpdir::TmpDir) -> Store {
    let mut s = Store::new();
    s.enable_seg_rows(d.path()).unwrap();
    s.hset(b"user:1", &[(b"name".as_slice(), b"ada".as_slice())]).unwrap();
    let sealed = s.seal_rows_to_seg(b"user", &[b"user:1".to_vec()]).unwrap().expect("sealed");
    assert_eq!(s.commit_row_eviction(&sealed), 1);
    s
}

fn is_seg_cold(s: &Store, key: &[u8]) -> bool {
    matches!(s.map.get(key).map(|e| &e.value), Some(Value::Cold(c)) if c.is_seg())
}

#[test]
fn a_snapshot_reads_a_segment_backed_row_through_its_pin() {
    let d = kevy_tmpdir::TmpDir::new("seg-cold-snapshot");
    let s = seg_backed(&d);
    let view = s.collect_snapshot();
    let mut rebuilt = Store::new();
    let mut seen = 0;
    view.each(|k, v, ttl| {
        assert!(matches!(v, Value::Cold(c) if c.is_seg()), "the view froze the stub");
        let hot = view.materialize_cold(k, v).expect("a stub materializes");
        rebuilt.load_value(k, &hot, ttl);
        seen += 1;
    });
    assert_eq!(seen, 1);
    assert_eq!(rebuilt.hget(b"user:1", b"name").unwrap(), Some(&b"ada"[..]));
    assert!(is_seg_cold(&s, b"user:1"), "the store's row stays cold");
}

#[test]
fn a_peek_reads_a_segment_backed_row_without_promoting_it() {
    let d = kevy_tmpdir::TmpDir::new("seg-cold-peek");
    let mut s = seg_backed(&d);
    let got = s.peek_hash_fields(b"user:1", &[b"name".as_slice(), b"none".as_slice()]);
    assert_eq!(got, Ok(Some(vec![Some(b"ada".to_vec()), None])));
    assert!(is_seg_cold(&s, b"user:1"));
}

#[test]
fn persistence_materializes_a_segment_backed_row_and_leaves_it_cold() {
    let d = kevy_tmpdir::TmpDir::new("seg-cold-materialize");
    let s = seg_backed(&d);
    let stub = s.map.get(b"user:1".as_slice()).map(|e| e.value.clone()).expect("a stub");
    let hot = s.materialize_cold(b"user:1", &stub).expect("a cold value materializes");
    let mut rebuilt = Store::new();
    rebuilt.load_value(b"user:1", &hot, None);
    assert_eq!(rebuilt.hget(b"user:1", b"name").unwrap(), Some(&b"ada"[..]));
    assert!(is_seg_cold(&s, b"user:1"));
}

#[test]
fn a_nearly_full_table_reserves_its_next_growth_from_the_tier() {
    let d = kevy_tmpdir::TmpDir::new("seg-cold-growth-reserve");
    let mut s = Store::new();
    s.enable_tiering(d.path(), u64::MAX).unwrap();
    let reserve = |s: &Store| s.tier.as_ref().expect("tiered").growth_reserve;
    s.set(b"k0", b"v".to_vec(), None, SetCondition::Always);
    s.tier_reserve_growth();
    assert_eq!(reserve(&s), 0, "room to spare");
    let mut i = 1;
    while s.map.room() > s.map.capacity() / 8 {
        s.set(format!("k{i}").as_bytes(), b"v".to_vec(), None, SetCondition::Always);
        i += 1;
    }
    s.tier_reserve_growth();
    let want = (s.map.grown_footprint() as u64).saturating_sub(s.keyspace_bytes);
    assert!(want > 0);
    assert_eq!(reserve(&s), want);
}

#[test]
fn a_demoted_string_is_lent_as_the_bytes_it_held() {
    let d = kevy_tmpdir::TmpDir::new("seg-cold-lent-string");
    let mut s = Store::new();
    s.enable_tiering(d.path(), u64::MAX).unwrap();
    let big = vec![b'b'; 3000];
    s.set(b"big", big.clone(), None, SetCondition::Always);
    // written in place, these hold their few bytes in the bulk form
    s.setrange(b"small", 0, b"hello").unwrap();
    s.setrange(b"digits", 0, b"42").unwrap();
    for k in [&b"big"[..], b"small", b"digits"] {
        assert!(s.debug_force_demote(k));
    }
    let lent = |s: &Store, k: &[u8]| s.get_shared_with(k, |v| v.map(<[u8]>::to_vec));
    assert_eq!(lent(&s, b"big"), Ok(Some(big)));
    // read back in the form a plain SET of the bytes takes
    assert_eq!(lent(&s, b"small"), Ok(Some(b"hello".to_vec())));
    assert_eq!(lent(&s, b"digits"), Ok(Some(b"42".to_vec())));
    for k in [&b"big"[..], b"small", b"digits"] {
        assert!(crate::tests_tier::is_cold(&s, k), "lent, not promoted");
    }
}
