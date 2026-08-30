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
    let missing: Vec<&&str> = ESTORE_OPS.iter().filter(|v| !table.contains(*v as &str)).collect();
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

/// The extension verbs' arity guards, which the wire differential added
/// and the dead-path atlas then reported as four new never-executed
/// regions apiece: a branch written and not exercised is worse than the
/// drift it was written to close.
///
/// The server's arity for both is -4 (a minimum of four), and it refuses a
/// shorter call in Redis's words before it looks at the catalog. These
/// assert the same sentence here, and that the boundary is where the
/// server puts it rather than one argument off.
#[test]
fn idx_query_and_count_refuse_a_short_call_the_way_the_server_does() {
    let s = mem_store();
    for verb in [&b"IDX.QUERY"[..], &b"IDX.COUNT"[..]] {
        let name = String::from_utf8_lossy(verb).to_lowercase();
        let want = format!("-ERR wrong number of arguments for '{name}' command\r\n");
        for argv in [vec![verb], vec![verb, &b"t"[..]], vec![verb, &b"t"[..], &b"WHERE"[..]]] {
            assert_eq!(
                String::from_utf8_lossy(&run(&s, &argv)),
                want,
                "{name} with {} argument(s)",
                argv.len()
            );
        }
    }

    // Four arguments clears the arity bar, so the answer must be about the
    // request rather than about its length — the guard must not swallow a
    // call it was never meant to reject.
    let four = run(&s, &[b"IDX.QUERY", b"t", b"WHERE", b"x"]);
    assert!(
        !String::from_utf8_lossy(&four).contains("wrong number of arguments"),
        "a four-argument IDX.QUERY is past the arity guard: {}",
        String::from_utf8_lossy(&four)
    );
}

/// COMPOSE and HYBRID are longer forms that clear the bar by their own
/// shape; the guard exempts them by name, and that exemption is a branch
/// too.
#[test]
fn idx_query_compose_and_hybrid_are_not_caught_by_the_arity_guard() {
    let s = mem_store();
    for form in [&b"COMPOSE"[..], &b"HYBRID"[..]] {
        let out = run(&s, &[b"IDX.QUERY", form]);
        assert!(
            !String::from_utf8_lossy(&out).contains("wrong number of arguments"),
            "{} is exempt from the arity guard: {}",
            String::from_utf8_lossy(form),
            String::from_utf8_lossy(&out)
        );
    }
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

/// Verbs this facade dispatches that `OP_TABLE` does not describe.
///
/// OP_TABLE is the KEYSPACE registry: write classification, notification
/// class, wake index, surfaces. Connection and pub/sub verbs have no
/// keyspace semantics to record and consistently have no row — PING,
/// ECHO, SELECT, QUIT, HELLO, SUBSCRIBE, CLIENT, COMMAND and INFO are
/// all absent from it, so the boundary is a design and not an oversight.
///
/// The ledger is EXACT in both directions below.
const NOT_KEYSPACE: &[&str] = &["ECHO", "PING", "PUBLISH"];

/// Every verb this facade dispatches has an OP_TABLE row, or is named
/// above as having no keyspace semantics to record.
///
/// Nothing held this direction. Every check around the registry starts
/// FROM it — each SERVER row must have a dispatch literal, each ledgered
/// gap must still be a hole — so a verb BOTH surfaces implement and the
/// registry never heard of was invisible to all of them at once.
///
/// `HPTTL` was exactly that: documented in VERB_META, dispatched by the
/// server's `dispatch_collections.rs` and by this crate's `hash.rs`,
/// sibling to four hash-TTL verbs that all have rows — and absent from
/// OP_TABLE, therefore outside the notify-class check, the replay check
/// and the surface-versus-dispatch check together. Found by asking the
/// question from the facade's side instead of the registry's.
#[test]
fn every_dispatched_verb_is_in_the_registry_or_named_as_outside_it() {
    use kevy_resp::ops_table::OP_TABLE;

    let facade: BTreeSet<&str> = DISPATCH_VERBS.iter().copied().collect();
    assert!(facade.len() > 100, "only {} dispatched verbs — the table did not load", facade.len());

    let rows: BTreeSet<&str> = OP_TABLE.iter().map(|o| o.name).collect();
    let named: BTreeSet<&str> = NOT_KEYSPACE.iter().copied().collect();

    let missing: Vec<&str> =
        facade.difference(&rows).filter(|v| !named.contains(*v)).copied().collect();
    assert!(
        missing.is_empty(),
        "{missing:?} are dispatched here with no OP_TABLE row and no NOT_KEYSPACE entry — \
         either the registry is missing them, or they carry no keyspace semantics and \
         belong in the ledger with that reason"
    );

    let healed: Vec<&str> = named.iter().filter(|v| rows.contains(*v)).copied().collect();
    assert!(
        healed.is_empty(),
        "{healed:?} are named as outside the registry but have a row now — drop them from \
         NOT_KEYSPACE so the ledger stays exact"
    );
}
