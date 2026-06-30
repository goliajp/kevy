//! Tests for the bonus ops in `ops_bonus.rs`
//! (kevy-embedded 1.8.0).

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- setnx ---------------------------------------------------------------

#[test]
fn setnx_succeeds_on_absent_key() {
    let s = s();
    assert!(s.setnx(b"k", b"v").unwrap());
    assert_eq!(s.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn setnx_vetoed_when_key_exists() {
    let s = s();
    s.set(b"k", b"existing").unwrap();
    assert!(!s.setnx(b"k", b"new").unwrap());
    assert_eq!(s.get(b"k").unwrap(), Some(b"existing".to_vec()));
}

// ---- incrbyfloat ---------------------------------------------------------

#[test]
fn incrbyfloat_initializes_at_zero() {
    let s = s();
    let v = s.incrbyfloat(b"k", 1.5).unwrap();
    assert!((v - 1.5).abs() < 1e-9, "got {v}");
}

#[test]
fn incrbyfloat_accumulates() {
    let s = s();
    s.incrbyfloat(b"k", 2.0).unwrap();
    let v = s.incrbyfloat(b"k", 3.5).unwrap();
    assert!((v - 5.5).abs() < 1e-9, "got {v}");
}

// ---- decr / decrby -------------------------------------------------------

#[test]
fn decr_initializes_negative_one() {
    let s = s();
    assert_eq!(s.decr(b"k").unwrap(), -1);
    assert_eq!(s.decr(b"k").unwrap(), -2);
}

#[test]
fn decrby_arbitrary_delta() {
    let s = s();
    assert_eq!(s.decrby(b"k", 5).unwrap(), -5);
    assert_eq!(s.decrby(b"k", -3).unwrap(), -2);
}

// ---- strlen + append -----------------------------------------------------

#[test]
fn strlen_basic_and_absent() {
    let s = s();
    assert_eq!(s.strlen(b"absent").unwrap(), 0);
    s.set(b"k", b"hello").unwrap();
    assert_eq!(s.strlen(b"k").unwrap(), 5);
}

#[test]
fn append_extends_and_returns_new_length() {
    let s = s();
    let n = s.append(b"k", b"abc").unwrap();
    assert_eq!(n, 3);
    let n = s.append(b"k", b"def").unwrap();
    assert_eq!(n, 6);
    assert_eq!(s.get(b"k").unwrap(), Some(b"abcdef".to_vec()));
}

// ---- hsetnx --------------------------------------------------------------

#[test]
fn hsetnx_succeeds_on_absent_field() {
    let s = s();
    assert!(s.hsetnx(b"h", b"f", b"v").unwrap());
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn hsetnx_vetoed_when_field_exists() {
    let s = s();
    s.hset(b"h", &[(b"f", b"existing")]).unwrap();
    assert!(!s.hsetnx(b"h", b"f", b"new").unwrap());
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(b"existing".to_vec()));
}

// ---- ttl_secs ------------------------------------------------------------

#[test]
fn ttl_secs_no_ttl_returns_negative_one() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    assert_eq!(s.ttl_secs(b"k"), -1);
}

#[test]
fn ttl_secs_absent_returns_negative_two() {
    let s = s();
    assert_eq!(s.ttl_secs(b"absent"), -2);
}

#[test]
fn ttl_secs_after_pexpire() {
    let s = s();
    s.set(b"k", b"v").unwrap();
    s.pexpire(b"k", 30_000).unwrap();
    let t = s.ttl_secs(b"k");
    assert!((1..=30).contains(&t), "expected 1..=30s, got {t}");
}
