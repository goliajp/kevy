//! Tests for the scan family in `ops_scan.rs`
//! (kevy-embedded 1.9.0).

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- scan ----------------------------------------------------------------

#[test]
fn scan_walks_full_keyspace_paged() {
    let s = s();
    for i in 0..10 {
        s.set(format!("k{i}").as_bytes(), b"v").unwrap();
    }
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut cursor = 0u64;
    let mut iterations = 0;
    loop {
        let (next, batch) = s.scan(cursor, None, 3);
        seen.extend(batch);
        if next == 0 {
            break;
        }
        cursor = next;
        iterations += 1;
        assert!(iterations < 100, "scan didn't terminate");
    }
    seen.sort();
    let mut expected: Vec<Vec<u8>> = (0..10).map(|i| format!("k{i}").into_bytes()).collect();
    expected.sort();
    assert_eq!(seen, expected);
}

#[test]
fn scan_pattern_filters() {
    let s = s();
    s.set(b"user:1", b"a").unwrap();
    s.set(b"user:2", b"b").unwrap();
    s.set(b"other:1", b"x").unwrap();
    let (next, batch) = s.scan(0, Some(b"user:*"), 100);
    assert_eq!(next, 0);
    let mut got = batch;
    got.sort();
    assert_eq!(got, vec![b"user:1".to_vec(), b"user:2".to_vec()]);
}

#[test]
fn scan_zero_count_makes_no_progress() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    let (next, batch) = s.scan(0, None, 0);
    assert_eq!(next, 0);
    assert!(batch.is_empty());
}

#[test]
fn keys_iter_yields_all() {
    let s = s();
    s.set(b"a", b"1").unwrap();
    s.set(b"b", b"2").unwrap();
    let mut got: Vec<Vec<u8>> = s.keys_iter(None).collect();
    got.sort();
    assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec()]);
}

// ---- hscan ---------------------------------------------------------------

#[test]
fn hscan_walks_fields_paged() {
    let s = s();
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..6)
        .map(|i| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
        .collect();
    let pair_refs: Vec<(&[u8], &[u8])> = pairs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
    s.hset(b"h", &pair_refs).unwrap();
    let mut seen: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let (next, batch) = s.hscan(b"h", cursor, 2).unwrap();
        seen.extend(batch);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    seen.sort();
    let mut expected = pairs.clone();
    expected.sort();
    assert_eq!(seen, expected);
}

#[test]
fn hash_iter_yields_all() {
    let s = s();
    s.hset(b"h", &[(b"f", b"v")]).unwrap();
    let got: Vec<(Vec<u8>, Vec<u8>)> = s.hash_iter(b"h").unwrap().collect();
    assert_eq!(got, vec![(b"f".to_vec(), b"v".to_vec())]);
}

// ---- zscan ---------------------------------------------------------------

#[test]
fn zscan_walks_members_paged() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")]).unwrap();
    let mut seen: Vec<(Vec<u8>, f64)> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let (next, batch) = s.zscan(b"z", cursor, 2).unwrap();
        seen.extend(batch);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    assert_eq!(
        seen,
        vec![
            (b"a".to_vec(), 1.0),
            (b"b".to_vec(), 2.0),
            (b"c".to_vec(), 3.0),
            (b"d".to_vec(), 4.0),
        ]
    );
}

#[test]
fn zset_iter_yields_all_in_score_order() {
    let s = s();
    s.zadd(b"z", &[(2.0, b"b"), (1.0, b"a")]).unwrap();
    let got: Vec<(Vec<u8>, f64)> = s.zset_iter(b"z").unwrap().collect();
    assert_eq!(got, vec![(b"a".to_vec(), 1.0), (b"b".to_vec(), 2.0)]);
}
