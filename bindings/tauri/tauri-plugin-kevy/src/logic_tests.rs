//! Unit tests for the command logic against a live embedded store.
//!
//! These exercise the exact functions the `#[tauri::command]`s delegate to, so
//! every command's real behavior (arg handling, the TTL branch, error mapping,
//! RESP decode, the empty-argv guard) is covered headlessly. The Tauri
//! transport wrappers around them are trivial delegations; the full IPC +
//! permission (ACL) path is exercised by the example app.

use super::*;
use kevy_embedded::{Config, Store};
use std::time::Duration;

fn store() -> Store {
    Store::open(Config::default()).unwrap()
}

#[test]
fn set_get_roundtrip_and_missing_is_none() {
    let s = store();
    set(&s, b"greeting", b"hi", None).unwrap();
    assert_eq!(get(&s, b"greeting").unwrap(), Some(b"hi".to_vec()));
    assert_eq!(get(&s, b"absent").unwrap(), None);
}

#[test]
fn set_with_ttl_expires() {
    let s = store();
    set(&s, b"k", b"v", Some(30)).unwrap();
    assert_eq!(get(&s, b"k").unwrap(), Some(b"v".to_vec()));
    std::thread::sleep(Duration::from_millis(60));
    s.tick(); // drive the manual reaper past the deadline.
    assert_eq!(get(&s, b"k").unwrap(), None);
}

#[test]
fn del_and_exists_count_repeats() {
    let s = store();
    set(&s, b"a", b"1", None).unwrap();
    set(&s, b"b", b"2", None).unwrap();
    let keys = vec![b"a".to_vec(), b"a".to_vec(), b"b".to_vec()];
    assert_eq!(exists(&s, &keys).unwrap(), 3);
    let del_keys = vec![b"a".to_vec(), b"b".to_vec(), b"nope".to_vec()];
    assert_eq!(del(&s, &del_keys).unwrap(), 2);
}

#[test]
fn incr_and_incr_by() {
    let s = store();
    assert_eq!(incr(&s, b"c", None).unwrap(), 1);
    assert_eq!(incr(&s, b"c", Some(10)).unwrap(), 11);
    assert_eq!(incr(&s, b"c", Some(-1)).unwrap(), 10);
}

#[test]
fn incr_on_non_integer_is_store_error() {
    let s = store();
    set(&s, b"str", b"abc", None).unwrap();
    let err = incr(&s, b"str", None).unwrap_err();
    // Serialises to { kind: "Store", store: "NotInteger", … }.
    let v = serde_json::to_value(err).unwrap();
    assert_eq!(v["kind"], "Store");
    assert_eq!(v["store"], "NotInteger");
}

#[test]
fn dbsize_and_flushall() {
    let s = store();
    set(&s, b"x", b"1", None).unwrap();
    assert_eq!(dbsize(&s), 1);
    flushall(&s).unwrap();
    assert_eq!(dbsize(&s), 0);
}

#[test]
fn raw_cmd_decodes_int_and_bulk() {
    let s = store();
    let argv = |parts: &[&[u8]]| parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>();

    let reply = raw_cmd(&s, &argv(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"])).unwrap();
    let v = serde_json::to_value(reply).unwrap();
    assert_eq!(v["type"], "int");
    assert_eq!(v["int"], 2);

    let reply = raw_cmd(&s, &argv(&[b"HGET", b"h", b"f1"])).unwrap();
    let v = serde_json::to_value(reply).unwrap();
    assert_eq!(v["type"], "bulk");
    let bytes: Vec<u8> = serde_json::from_value(v["bytes"].clone()).unwrap();
    assert_eq!(bytes, b"v1".to_vec());
}

#[test]
fn raw_cmd_wrongtype_is_error_frame_not_transport_error() {
    let s = store();
    set(&s, b"s", b"str", None).unwrap();
    let argv = vec![b"LPUSH".to_vec(), b"s".to_vec(), b"x".to_vec()];
    let reply = raw_cmd(&s, &argv).unwrap(); // successful round-trip …
    let v = serde_json::to_value(reply).unwrap();
    assert_eq!(v["type"], "error"); // … carrying an error frame.
    let text: Vec<u8> = serde_json::from_value(v["bytes"].clone()).unwrap();
    assert!(String::from_utf8_lossy(&text).contains("WRONGTYPE"));
}

#[test]
fn raw_cmd_empty_argv_is_invalid_input() {
    let s = store();
    let err = raw_cmd(&s, &[]).unwrap_err();
    let v = serde_json::to_value(err).unwrap();
    assert_eq!(v["kind"], "InvalidInput");
}

#[test]
fn publish_counts_no_subscribers() {
    let s = store();
    assert_eq!(publish(&s, b"room", b"hi"), 0);
}
