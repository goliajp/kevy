//! Verb ids: dispatching by the id resolve() hands out answers exactly as
//! dispatching by name does, and only the verbs with a direct body get one.

use kevy_resp::Argv;
use kevy_rt::{Commands, RespVersion, VerbId};
use kevy_store::Store;

use crate::KevyCommands;
use crate::verb_meta::VERB_META;

fn argv(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

/// A store with a string, a hash and an expiring string in it.
fn seeded(c: &KevyCommands) -> Store {
    let mut s = Store::new();
    for parts in [
        &[&b"SET"[..], b"s", b"v"][..],
        &[b"HSET", b"h", b"f", b"1"],
        &[b"SET", b"t", b"old", b"EX", b"100"],
    ] {
        c.dispatch(&mut s, &argv(parts));
    }
    s
}

fn snapshot(c: &KevyCommands, s: &mut Store) -> Vec<Vec<u8>> {
    ["s", "h", "t", "n", "k"]
        .iter()
        .flat_map(|k| {
            [
                c.dispatch(s, &argv(&[b"TYPE", k.as_bytes()])),
                c.dispatch(s, &argv(&[b"DUMP", k.as_bytes()])),
                c.dispatch(s, &argv(&[b"TTL", k.as_bytes()])),
            ]
        })
        .collect()
}

#[test]
fn only_get_and_set_resolve_to_an_id() {
    let c = KevyCommands::new();
    for m in VERB_META {
        let upper = m.name.as_bytes();
        let lower = m.name.to_ascii_lowercase();
        for name in [upper, lower.as_bytes()] {
            let verb = c.resolve(&argv(&[name, b"k", b"v"])).verb;
            let want = match m.name {
                "GET" => crate::dispatch::VERB_GET,
                "SET" => crate::dispatch::VERB_SET,
                _ => VerbId::UNKNOWN,
            };
            assert_eq!(verb, want, "{}", m.name);
        }
    }
}

#[test]
fn dispatch_by_id_answers_as_dispatch_by_name() {
    let c = KevyCommands::new();
    let cases: &[&[&[u8]]] = &[
        &[b"GET", b"s"],
        &[b"get", b"s"],
        &[b"GET", b"n"],
        &[b"GET", b"h"],
        &[b"GET", b"t"],
        &[b"GET"],
        &[b"GET", b"s", b"extra"],
        &[b"SET", b"k", b"v"],
        &[b"set", b"s", b"new"],
        &[b"SET", b"s", b"new", b"NX"],
        &[b"SET", b"n", b"new", b"XX"],
        &[b"SET", b"s", b"new", b"GET"],
        &[b"SET", b"h", b"new", b"GET"],
        &[b"SET", b"t", b"new", b"KEEPTTL"],
        &[b"SET", b"k", b"v", b"EX", b"10"],
        &[b"SET", b"k", b"v", b"EX", b"-1"],
        &[b"SET", b"k", b"v", b"BOGUS"],
        &[b"SET", b"k"],
        &[b"SET", b"k", b"12345678901234567890123"],
    ];
    for parts in cases {
        let args = argv(parts);
        let verb = c.resolve(&args).verb;
        assert_ne!(verb, VerbId::UNKNOWN, "{parts:?} takes the id path");
        for proto in [RespVersion::V2, RespVersion::V3] {
            let (mut by_name, mut by_id) = (seeded(&c), seeded(&c));
            let mut want = Vec::new();
            match proto {
                RespVersion::V2 => c.dispatch_into(&mut by_name, &args, &mut want),
                RespVersion::V3 => c.dispatch_into_resp3(&mut by_name, &args, &mut want),
            }
            let mut got = Vec::new();
            c.dispatch_verb_into(&mut by_id, &args, verb, proto, &mut got);
            assert_eq!(got, want, "{parts:?} {proto:?}: reply");
            assert_eq!(
                snapshot(&c, &mut by_id),
                snapshot(&c, &mut by_name),
                "{parts:?} {proto:?}: keyspace"
            );
        }
    }
}

#[test]
fn an_unknown_id_dispatches_by_name() {
    let c = KevyCommands::new();
    let mut s = Store::new();
    let mut out = Vec::new();
    let args = argv(&[b"INCR", b"n"]);
    c.dispatch_verb_into(&mut s, &args, VerbId::UNKNOWN, RespVersion::V2, &mut out);
    assert_eq!(out, b":1\r\n");
}

#[test]
fn a_scoped_set_by_id_is_redirected_like_one_by_name() {
    let mut cfg = kevy_config::Config::default();
    cfg.cluster.node_id = "A".to_string();
    cfg.cluster.peers =
        kevy_config::PeerEntry::parse_list("A@127.0.0.1:6004,B@10.0.0.99:6004").unwrap();
    cfg.cluster.scopes = kevy_config::ScopeEntry::parse_list("app:=B").unwrap();
    let state = crate::RuntimeState::new(std::sync::Arc::new(cfg), std::path::PathBuf::new(), 1);
    let c = KevyCommands::with_state(std::sync::Arc::new(state.unwrap()));
    for key in [&b"app:foo"[..], b"other:k"] {
        let args = argv(&[b"SET", key, b"v"]);
        let verb = c.resolve(&args).verb;
        assert_eq!(verb, crate::dispatch::VERB_SET);
        let (mut by_name, mut by_id) = (Store::new(), Store::new());
        let want = c.dispatch(&mut by_name, &args);
        let mut got = Vec::new();
        c.dispatch_verb_into(&mut by_id, &args, verb, RespVersion::V2, &mut got);
        assert_eq!(got, want, "{}", String::from_utf8_lossy(key));
        assert_eq!(by_id.dbsize(), by_name.dbsize());
    }
}

#[test]
fn a_set_into_a_moving_scope_is_quiesced_by_id_and_by_name() {
    let mut cfg = kevy_config::Config::default();
    cfg.cluster.node_id = "A".to_string();
    cfg.cluster.peers =
        kevy_config::PeerEntry::parse_list("A@127.0.0.1:6004,B@10.0.0.99:6004").unwrap();
    cfg.cluster.scopes = kevy_config::ScopeEntry::parse_list("app:=A").unwrap();
    let state = crate::RuntimeState::new(std::sync::Arc::new(cfg), std::path::PathBuf::new(), 1);
    let c = KevyCommands::with_state(std::sync::Arc::new(state.unwrap()));
    c.state().scope.migration_start(b"app:".to_vec(), "A".into(), "B".into()).unwrap();
    let args = argv(&[b"SET", b"app:foo", b"v"]);
    let verb = c.resolve(&args).verb;
    let (mut by_name, mut by_id) = (Store::new(), Store::new());
    assert_eq!(c.dispatch(&mut by_name, &args), b"-QUIESCED migrating to 10.0.0.99:6004\r\n");
    let mut got = Vec::new();
    c.dispatch_verb_into(&mut by_id, &args, verb, RespVersion::V2, &mut got);
    assert_eq!(got, b"-QUIESCED migrating to 10.0.0.99:6004\r\n");
    assert_eq!(by_id.dbsize() + by_name.dbsize(), 0, "neither wrote");
}
