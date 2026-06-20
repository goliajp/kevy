//! T3.18 e2e: a write to the "wrong" embed/server returns
//! `-MISDIRECTED writer is <host:port>` over real RESP. Covers the
//! routing wire path through `kevy::dispatch` — the cement check
//! in `dispatch_with_proto` + `scope_integration::route_write` +
//! `encode_misdirected`.
//!
//! Single-Runtime test: this node is configured with scopes that
//! name a DIFFERENT node-id as writer, so every write to a key
//! under those scopes must answer `-MISDIRECTED`. The actual
//! "follow to correct writer" path is tested by
//! `kevy_cluster_rw::parse_misdirected` unit tests + the smoke
//! test in `tests/scope_move_e2e.rs`.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use kevy_config::Config;
use kevy_resp::Argv;
use kevy_store::Store;

fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

#[test]
fn write_to_non_writer_node_returns_misdirected() {
    // Self = A, but the `app:` scope's writer = B. Writes to
    // `app:*` keys must MISDIRECT to B's address.
    let mut cfg = Config::default();
    cfg.cluster.node_id = "A".to_string();
    cfg.cluster.peers = kevy_config::PeerEntry::parse_list(
        "A@127.0.0.1:6004,B@10.0.0.99:6004",
    )
    .unwrap();
    cfg.cluster.scopes = kevy_config::ScopeEntry::parse_list("app:=B").unwrap();
    kevy::config_init(Arc::new(cfg.clone()));
    kevy::install_scope_integration_for_test(&cfg).expect("install scope_integration");

    let mut store = Store::new();
    // Write to a key under the `app:` prefix — should be redirected
    // because self_node_id (A) is not the declared writer (B).
    let reply = kevy::dispatch(&mut store, &argv(&[b"SET", b"app:foo", b"v"]));
    let s = String::from_utf8_lossy(&reply);
    assert!(
        s.starts_with("-MISDIRECTED"),
        "expected -MISDIRECTED, got: {s:?}",
    );
    assert!(
        s.contains("10.0.0.99:6004"),
        "should name B's host:port: {s:?}",
    );
    // The store must NOT have applied the write locally.
    assert_eq!(
        store.get(b"app:foo").map(|v| v.map(|c| c.into_owned())),
        Ok(None),
        "rejected write must not land in the store",
    );

    // A key OUTSIDE the scope behaves normally (no MISDIRECTED).
    let reply = kevy::dispatch(&mut store, &argv(&[b"SET", b"other:k", b"v"]));
    let s = String::from_utf8_lossy(&reply);
    assert!(s.starts_with('+') || s.starts_with(':'), "{s:?}");
    assert_eq!(
        store.get(b"other:k").map(|v| v.map(|c| c.into_owned())),
        Ok(Some(b"v".to_vec())),
    );
}

#[test]
fn read_to_non_writer_node_is_not_misdirected() {
    // Reads (`GET`) are NOT scope-routed — the redirection is for
    // writes only. A reader node should be able to serve reads
    // even when it doesn't own the scope (the writer-replication
    // path eventually makes the data appear here via embed-as-
    // read-replica / server replication — orthogonal to T3.x).
    // This test only verifies the dispatch DOESN'T MISDIRECT on
    // reads.
    let mut cfg = Config::default();
    cfg.cluster.node_id = "A".to_string();
    cfg.cluster.peers = kevy_config::PeerEntry::parse_list(
        "A@127.0.0.1:6004,B@10.0.0.99:6004",
    )
    .unwrap();
    cfg.cluster.scopes = kevy_config::ScopeEntry::parse_list("app:=B").unwrap();
    kevy::config_init(Arc::new(cfg.clone()));
    kevy::install_scope_integration_for_test(&cfg).expect("install scope_integration");

    let mut store = Store::new();
    let reply = kevy::dispatch(&mut store, &argv(&[b"GET", b"app:nonexistent"]));
    let s = String::from_utf8_lossy(&reply);
    assert!(
        !s.starts_with("-MISDIRECTED"),
        "reads must not be redirected: {s:?}",
    );
}
