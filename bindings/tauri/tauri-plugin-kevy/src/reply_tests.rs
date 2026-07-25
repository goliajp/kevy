//! Unit tests for RESP → `JsonReply` decoding, fed real bytes from a live
//! embedded store's `dispatch_argv` (not hand-rolled wire strings).

use super::decode;
use kevy_embedded::{Config, Store};
use serde_json::Value;

/// Run `argv` through a fresh store and decode the reply to a JSON value.
fn run(store: &Store, argv: &[&[u8]]) -> Value {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    store.dispatch_argv(&owned, &mut out);
    let reply = decode(&out).expect("decode");
    serde_json::to_value(reply).expect("to_value")
}

#[test]
fn simple_int_bulk_nil_array_shapes() {
    let s = Store::open(Config::default()).unwrap();

    // +OK simple string.
    let v = run(&s, &[b"SET", b"k", b"hello"]);
    assert_eq!(v["type"], "simple");
    assert_eq!(String::from_utf8(serde_json::from_value(v["bytes"].clone()).unwrap()).unwrap(), "OK");

    // $len bulk.
    let v = run(&s, &[b"GET", b"k"]);
    assert_eq!(v["type"], "bulk");
    let bytes: Vec<u8> = serde_json::from_value(v["bytes"].clone()).unwrap();
    assert_eq!(bytes, b"hello".to_vec());

    // $-1 nil (missing key).
    let v = run(&s, &[b"GET", b"absent"]);
    assert_eq!(v["type"], "nil");

    // :N integer.
    let v = run(&s, &[b"RPUSH", b"list", b"a", b"b", b"c"]);
    assert_eq!(v["type"], "int");
    assert_eq!(v["int"], 3);

    // *N array.
    let v = run(&s, &[b"LRANGE", b"list", b"0", b"-1"]);
    assert_eq!(v["type"], "array");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    assert_eq!(v["items"][0]["type"], "bulk");
}

#[test]
fn error_frame_decodes_as_error_reply() {
    let s = Store::open(Config::default()).unwrap();
    run(&s, &[b"SET", b"str", b"v"]);
    // LPUSH on a string -> -WRONGTYPE ... error frame.
    let v = run(&s, &[b"LPUSH", b"str", b"x"]);
    assert_eq!(v["type"], "error");
    let text: Vec<u8> = serde_json::from_value(v["bytes"].clone()).unwrap();
    assert!(String::from_utf8_lossy(&text).contains("WRONGTYPE"));
}
