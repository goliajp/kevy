//! Tests for the Phase 2 mailrs-feedback ops in `ops_p2.rs`.

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- hash mass-getters ----------------------------------------------------

#[test]
fn hgetall_returns_pairs() {
    let s = s();
    s.hset(b"t:1", &[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]).unwrap();
    let mut got = s.hgetall(b"t:1").unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec())
        ]
    );
}

#[test]
fn hgetall_empty_on_absent_key() {
    let s = s();
    assert!(s.hgetall(b"absent").unwrap().is_empty());
}

#[test]
fn hexists_hits_and_misses() {
    let s = s();
    s.hset(b"t", &[(b"f", b"v")]).unwrap();
    assert!(s.hexists(b"t", b"f").unwrap());
    assert!(!s.hexists(b"t", b"x").unwrap());
    assert!(!s.hexists(b"absent", b"f").unwrap());
}

#[test]
fn hlen_counts_fields() {
    let s = s();
    s.hset(b"t", &[(b"a", b"1"), (b"b", b"2")]).unwrap();
    assert_eq!(s.hlen(b"t").unwrap(), 2);
    assert_eq!(s.hlen(b"absent").unwrap(), 0);
}

#[test]
fn hkeys_and_hvals_enumerate() {
    let s = s();
    s.hset(b"t", &[(b"a", b"1"), (b"b", b"2")]).unwrap();
    let mut keys = s.hkeys(b"t").unwrap();
    keys.sort();
    assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
    let mut vals = s.hvals(b"t").unwrap();
    vals.sort();
    assert_eq!(vals, vec![b"1".to_vec(), b"2".to_vec()]);
}

#[test]
fn hmget_per_field_option() {
    let s = s();
    s.hset(b"t", &[(b"a", b"1"), (b"b", b"2")]).unwrap();
    let got = s.hmget(b"t", &[b"a", b"x", b"b"]).unwrap();
    assert_eq!(
        got,
        vec![Some(b"1".to_vec()), None, Some(b"2".to_vec())]
    );
}

#[test]
fn hincrby_atomic_increment() {
    let s = s();
    s.hset(b"c", &[(b"n", b"10")]).unwrap();
    assert_eq!(s.hincrby(b"c", b"n", 3).unwrap(), 13);
    assert_eq!(s.hincrby(b"c", b"n", -5).unwrap(), 8);
    // new field starts at 0.
    assert_eq!(s.hincrby(b"c", b"new", 7).unwrap(), 7);
}

// ---- zset range + atomic incr --------------------------------------------

#[test]
fn zrange_asc_with_scores() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c")]).unwrap();
    let got = s.zrange(b"z", 0, -1).unwrap();
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), 1.0),
            (b"b".to_vec(), 2.0),
            (b"c".to_vec(), 3.0)
        ]
    );
}

#[test]
fn zrevrange_desc_with_scores() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c")]).unwrap();
    let got = s.zrevrange(b"z", 0, -1).unwrap();
    assert_eq!(
        got,
        vec![
            (b"c".to_vec(), 3.0),
            (b"b".to_vec(), 2.0),
            (b"a".to_vec(), 1.0)
        ]
    );
}

#[test]
fn zrevrange_subset() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")]).unwrap();
    let got = s.zrevrange(b"z", 0, 1).unwrap();
    assert_eq!(got, vec![(b"d".to_vec(), 4.0), (b"c".to_vec(), 3.0)]);
}

#[test]
fn zrange_by_score_inclusive() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")]).unwrap();
    let got = s.zrange_by_score(b"z", 2.0, 3.0).unwrap();
    assert_eq!(
        got,
        vec![(b"b".to_vec(), 2.0), (b"c".to_vec(), 3.0)]
    );
}

#[test]
fn zincrby_atomic() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"x")]).unwrap();
    assert!((s.zincrby(b"z", 2.5, b"x").unwrap() - 3.5).abs() < 1e-9);
    // new member starts at 0.
    assert!((s.zincrby(b"z", 7.0, b"new").unwrap() - 7.0).abs() < 1e-9);
}

// ---- list slice + index --------------------------------------------------

#[test]
fn lrange_basic() {
    let s = s();
    s.rpush(b"l", &[b"a", b"b", b"c", b"d"]).unwrap();
    assert_eq!(
        s.lrange(b"l", 0, -1).unwrap(),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    assert_eq!(
        s.lrange(b"l", 1, 2).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
}

#[test]
fn lindex_positive_and_negative() {
    let s = s();
    s.rpush(b"l", &[b"a", b"b", b"c"]).unwrap();
    assert_eq!(s.lindex(b"l", 0).unwrap(), Some(b"a".to_vec()));
    assert_eq!(s.lindex(b"l", -1).unwrap(), Some(b"c".to_vec()));
    assert_eq!(s.lindex(b"l", 99).unwrap(), None);
}

#[test]
fn lrem_removes_n_from_head() {
    let s = s();
    s.rpush(b"l", &[b"x", b"y", b"x", b"z", b"x"]).unwrap();
    assert_eq!(s.lrem(b"l", 2, b"x").unwrap(), 2);
    assert_eq!(
        s.lrange(b"l", 0, -1).unwrap(),
        vec![b"y".to_vec(), b"z".to_vec(), b"x".to_vec()]
    );
}

// ---- string single-call atomic -------------------------------------------

#[test]
fn getset_returns_previous() {
    let s = s();
    assert_eq!(s.getset(b"k", b"v1").unwrap(), None);
    assert_eq!(s.getset(b"k", b"v2").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(s.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn getdel_returns_and_removes() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    assert_eq!(s.getdel(b"k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s.get(b"k").unwrap(), None);
    assert_eq!(s.getdel(b"absent").unwrap(), None);
}
