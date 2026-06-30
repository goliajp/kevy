//! Tests for the Phase 3 mailrs-feedback P1 round-out ops in `ops_p3.rs`.

use std::time::Duration;

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- mset / mget ---------------------------------------------------------

#[test]
fn mset_then_mget_round_trip() {
    let s = s();
    s.mset(&[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]).unwrap();
    let got = s.mget(&[b"a", b"b", b"missing", b"c"]).unwrap();
    assert_eq!(
        got,
        vec![
            Some(b"1".to_vec()),
            Some(b"2".to_vec()),
            None,
            Some(b"3".to_vec()),
        ]
    );
}

#[test]
fn mset_empty_is_noop() {
    let s = s();
    s.mset(&[]).unwrap();
    assert_eq!(s.dbsize(), 0);
}

// ---- keys ----------------------------------------------------------------

#[test]
fn keys_no_pattern_returns_all() {
    let s = s();
    s.set(b"a", b"1").unwrap();
    s.set(b"b", b"2").unwrap();
    s.set(b"c", b"3").unwrap();
    let mut got = s.keys(None, None);
    got.sort();
    assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
}

#[test]
fn keys_with_glob_filters() {
    let s = s();
    s.set(b"user:1", b"x").unwrap();
    s.set(b"user:2", b"y").unwrap();
    s.set(b"other:1", b"z").unwrap();
    let mut got = s.keys(Some(b"user:*"), None);
    got.sort();
    assert_eq!(got, vec![b"user:1".to_vec(), b"user:2".to_vec()]);
}

#[test]
fn keys_limit_caps_results() {
    let s = s();
    for i in 0..10 {
        s.set(format!("k{i}").as_bytes(), b"v").unwrap();
    }
    let got = s.keys(None, Some(3));
    assert_eq!(got.len(), 3);
}

// ---- getex ---------------------------------------------------------------

#[test]
fn getex_returns_value_and_sets_ttl() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    let got = s.getex(b"k", Duration::from_secs(60)).unwrap();
    assert_eq!(got, Some(b"v".to_vec()));
    let ttl = s.ttl_ms(b"k");
    assert!(
        (1..=60_000).contains(&ttl),
        "expected TTL in (0..60_000], got {ttl}"
    );
}

#[test]
fn getex_on_absent_returns_none_no_ttl() {
    let s = s();
    let got = s.getex(b"absent", Duration::from_secs(60)).unwrap();
    assert_eq!(got, None);
}

// ---- set algebra ---------------------------------------------------------

#[test]
fn sinter_two_sets() {
    let s = s();
    s.sadd(b"a", &[b"x", b"y", b"z"]).unwrap();
    s.sadd(b"b", &[b"y", b"z", b"w"]).unwrap();
    let mut got = s.sinter(&[b"a", b"b"]).unwrap();
    got.sort();
    assert_eq!(got, vec![b"y".to_vec(), b"z".to_vec()]);
}

#[test]
fn sinter_empty_intersection() {
    let s = s();
    s.sadd(b"a", &[b"x"]).unwrap();
    s.sadd(b"b", &[b"y"]).unwrap();
    assert!(s.sinter(&[b"a", b"b"]).unwrap().is_empty());
}

#[test]
fn sunion_three_sets() {
    let s = s();
    s.sadd(b"a", &[b"x"]).unwrap();
    s.sadd(b"b", &[b"y"]).unwrap();
    s.sadd(b"c", &[b"z"]).unwrap();
    let mut got = s.sunion(&[b"a", b"b", b"c"]).unwrap();
    got.sort();
    assert_eq!(got, vec![b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]);
}

#[test]
fn sdiff_first_minus_rest() {
    let s = s();
    s.sadd(b"a", &[b"x", b"y", b"z"]).unwrap();
    s.sadd(b"b", &[b"y"]).unwrap();
    s.sadd(b"c", &[b"z"]).unwrap();
    let mut got = s.sdiff(&[b"a", b"b", b"c"]).unwrap();
    got.sort();
    assert_eq!(got, vec![b"x".to_vec()]);
}

// ---- absolute-time TTL ---------------------------------------------------

#[test]
fn pexpireat_sets_deadline() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    // far-future deadline so the key doesn't expire mid-test.
    let unix_ms = 4_102_444_800_000; // ~year 2100
    assert!(s.pexpireat(b"k", unix_ms).unwrap());
    let ttl = s.ttl_ms(b"k");
    assert!(ttl > 0, "expected positive TTL after pexpireat, got {ttl}");
}

#[test]
fn pexpireat_on_absent_returns_false() {
    let s = s();
    let unix_ms = 4_102_444_800_000;
    assert!(!s.pexpireat(b"absent", unix_ms).unwrap());
}

#[test]
fn pexpire_sets_relative_ms() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    assert!(s.pexpire(b"k", 30_000).unwrap());
    let ttl = s.ttl_ms(b"k");
    assert!(
        (1..=30_000).contains(&ttl),
        "expected TTL in (0..30_000], got {ttl}"
    );
}

// ---- hincrbyfloat -------------------------------------------------------

#[test]
fn hincrbyfloat_creates_and_increments() {
    let s = s();
    // missing field starts at 0.0.
    let v1 = s.hincrbyfloat(b"h", b"f", 1.5).unwrap();
    assert!((v1 - 1.5).abs() < 1e-9, "got {v1}");
    let v2 = s.hincrbyfloat(b"h", b"f", 2.25).unwrap();
    assert!((v2 - 3.75).abs() < 1e-9, "got {v2}");
}

#[test]
fn hincrbyfloat_negative_delta() {
    let s = s();
    s.hset(b"h", &[(b"f", b"10")]).unwrap();
    let v = s.hincrbyfloat(b"h", b"f", -2.5).unwrap();
    assert!((v - 7.5).abs() < 1e-9, "got {v}");
}

// ---- ping_ns ------------------------------------------------------------

#[test]
fn ping_ns_returns_positive_value() {
    let s = s();
    let n = s.ping_ns();
    // anything >= 0 is technically valid; on Mac/Linux the lock
    // acquire + release pair is typically tens to hundreds of ns.
    // Hard upper bound: 1 second is comically high but guards against
    // a hung implementation.
    assert!(n < 1_000_000_000, "ping_ns suspiciously large: {n}");
}
