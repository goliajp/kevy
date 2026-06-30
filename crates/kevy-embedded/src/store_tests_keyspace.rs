//! Tests for keyspace cross-key ops in `ops_keyspace.rs`
//! (kevy-embedded 1.12.0).

use std::time::Duration;

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- copy ---------------------------------------------------------------

#[test]
fn copy_absent_src_returns_false() {
    let s = s();
    assert!(!s.copy(b"absent", b"dst", false).unwrap());
}

#[test]
fn copy_to_new_dst_succeeds() {
    let s = s();
    s.set(b"src", b"value").unwrap();
    assert!(s.copy(b"src", b"dst", false).unwrap());
    assert_eq!(s.get(b"src").unwrap(), Some(b"value".to_vec()));
    assert_eq!(s.get(b"dst").unwrap(), Some(b"value".to_vec()));
}

#[test]
fn copy_to_existing_dst_vetoes_without_replace() {
    let s = s();
    s.set(b"src", b"v1").unwrap();
    s.set(b"dst", b"existing").unwrap();
    assert!(!s.copy(b"src", b"dst", false).unwrap());
    assert_eq!(s.get(b"dst").unwrap(), Some(b"existing".to_vec()));
}

#[test]
fn copy_with_replace_overwrites() {
    let s = s();
    s.set(b"src", b"v1").unwrap();
    s.set(b"dst", b"existing").unwrap();
    assert!(s.copy(b"src", b"dst", true).unwrap());
    assert_eq!(s.get(b"dst").unwrap(), Some(b"v1".to_vec()));
}

#[test]
fn copy_preserves_ttl_on_dst() {
    let s = s();
    s.set(b"src", b"v").unwrap();
    s.pexpire(b"src", 60_000).unwrap();
    assert!(s.copy(b"src", b"dst", false).unwrap());
    let ttl = s.ttl_ms(b"dst");
    assert!(
        (1..=60_000).contains(&ttl),
        "expected TTL preserved on dst, got {ttl}"
    );
}

// ---- randomkey ----------------------------------------------------------

#[test]
fn randomkey_empty_returns_none() {
    let s = s();
    assert_eq!(s.randomkey(), None);
}

#[test]
fn randomkey_picks_an_existing_key() {
    let s = s();
    s.set(b"a", b"1").unwrap();
    s.set(b"b", b"2").unwrap();
    s.set(b"c", b"3").unwrap();
    let k = s.randomkey().unwrap();
    assert!(
        matches!(k.as_slice(), b"a" | b"b" | b"c"),
        "got unexpected key: {:?}",
        String::from_utf8_lossy(&k)
    );
}

// ---- unlink -------------------------------------------------------------

#[test]
fn unlink_deletes_like_del() {
    let s = s();
    s.set(b"a", b"1").unwrap();
    s.set(b"b", b"2").unwrap();
    let removed = s.unlink(&[b"a", b"b", b"missing"]).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(s.dbsize(), 0);
}

// ---- touch --------------------------------------------------------------

#[test]
fn touch_counts_existing_keys() {
    let s = s();
    s.set(b"a", b"1").unwrap();
    s.set(b"b", b"2").unwrap();
    let n = s.touch(&[b"a", b"missing", b"b"]).unwrap();
    assert_eq!(n, 2);
}

#[test]
fn touch_zero_for_all_missing() {
    let s = s();
    assert_eq!(s.touch(&[b"x", b"y"]).unwrap(), 0);
}

// ---- copy with sub-second TTL -----------------------------------------

#[test]
fn copy_short_ttl_survives() {
    let s = s();
    s.set_with_ttl(b"src", b"v", Duration::from_secs(2)).unwrap();
    assert!(s.copy(b"src", b"dst", false).unwrap());
    let ttl = s.ttl_ms(b"dst");
    assert!(ttl > 0 && ttl <= 2000, "got {ttl}");
}
