//! Two-way parity gate for the full-surface dispatcher:
//!
//! 1. every `ESTORE_OPS` verb must have a dispatch arm (probing each
//!    verb with zero arguments must NOT answer unknown-command);
//! 2. `ESTORE_OPS` ⊆ `DISPATCH_VERBS` (the arm table is a superset —
//!    conn-face extras like PING are allowed, missing rows are not).
//!
//! Plus spot checks that the encoding matches the server wording the
//! oracle test verifies end to end.

use std::collections::BTreeSet;

use crate::op_manifest::ESTORE_OPS;
use crate::{Config, Store};

use super::DISPATCH_VERBS;

fn mem_store() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).expect("open in-memory store")
}

fn run(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    super::dispatch(s, &owned, &mut out);
    out
}

#[test]
fn every_estore_op_has_a_dispatch_arm() {
    let s = mem_store();
    let mut missing = Vec::new();
    for verb in ESTORE_OPS {
        let reply = run(&s, &[verb.as_bytes()]);
        if reply.starts_with(b"-ERR unknown command") {
            missing.push(*verb);
        }
    }
    assert!(missing.is_empty(), "ESTORE_OPS verbs without a dispatch arm: {missing:?}");
}

#[test]
fn estore_ops_is_subset_of_dispatch_verbs() {
    let table: BTreeSet<&str> = DISPATCH_VERBS.iter().copied().collect();
    let missing: Vec<&&str> =
        ESTORE_OPS.iter().filter(|v| !table.contains(*v as &str)).collect();
    assert!(missing.is_empty(), "ESTORE_OPS verbs missing from DISPATCH_VERBS: {missing:?}");
}

#[test]
fn every_dispatch_verb_probes_as_known() {
    // The table itself must not list verbs the router doesn't own.
    let s = mem_store();
    for verb in DISPATCH_VERBS {
        let reply = run(&s, &[verb.as_bytes()]);
        assert!(
            !reply.starts_with(b"-ERR unknown command"),
            "DISPATCH_VERBS lists {verb} but the router answers unknown-command"
        );
    }
}

#[test]
fn verb_matching_is_case_insensitive() {
    let s = mem_store();
    assert_eq!(run(&s, &[b"set", b"k", b"v"]), b"+OK\r\n");
    assert_eq!(run(&s, &[b"gEt", b"k"]), b"$1\r\nv\r\n");
}

#[test]
fn unknown_verb_reports_original_spelling() {
    let s = mem_store();
    assert_eq!(run(&s, &[b"NoSuchVerbX"]), b"-ERR unknown command 'NoSuchVerbX'\r\n".to_vec());
}

#[test]
fn arity_and_type_errors_use_server_wording() {
    let s = mem_store();
    assert_eq!(
        run(&s, &[b"GET"]),
        b"-ERR wrong number of arguments for 'get' command\r\n".to_vec()
    );
    assert_eq!(run(&s, &[b"SET", b"k", b"v"]), b"+OK\r\n");
    assert_eq!(
        run(&s, &[b"LPUSH", b"k", b"x"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
    assert_eq!(
        run(&s, &[b"INCR", b"k"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn writes_flow_through_dispatch() {
    let s = mem_store();
    assert_eq!(run(&s, &[b"HSET", b"h", b"f", b"v"]), b":1\r\n");
    assert_eq!(run(&s, &[b"HGET", b"h", b"f"]), b"$1\r\nv\r\n");
    assert_eq!(run(&s, &[b"ZADD", b"z", b"1.5", b"m"]), b":1\r\n");
    assert_eq!(run(&s, &[b"ZSCORE", b"z", b"m"]), b"$3\r\n1.5\r\n");
    assert_eq!(run(&s, &[b"DEL", b"h", b"z"]), b":2\r\n");
}

#[test]
fn dispatch_argv_facade_reaches_the_full_surface() {
    // The public facade must route through THIS dispatcher (writes
    // included), not the read-only listener whitelist.
    let s = mem_store();
    let argv = vec![b"SET".to_vec(), b"facade".to_vec(), b"1".to_vec()];
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    assert_eq!(out, b"+OK\r\n");
    assert_eq!(s.get(b"facade").unwrap(), Some(b"1".to_vec()));
}
