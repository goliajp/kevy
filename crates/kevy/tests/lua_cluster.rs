//! v1.27 P7d — EVAL / EVALSHA cross-slot enforcement when cluster
//! mode is enabled.
//!
//! Separate test binary because we install a cluster-enabled global
//! Config (`kevy::config_init`) which is process-wide. Putting these
//! cases alongside `lua_eval.rs` would race the default-config tests
//! that file relies on.

use kevy_config::{ClusterSection, Config};
use kevy_resp::Argv;
use kevy_store::Store;
use std::sync::Arc;

fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

fn install_cluster_enabled() {
    // Use `replace` so re-running individual tests in the same
    // binary is safe (init can only be called once globally).
    let mut cfg = Config::default();
    cfg.cluster = ClusterSection {
        enabled: true,
        ..ClusterSection::default()
    };
    // Try init first (idempotent across the test-binary lifetime);
    // fall back to replace if another test already initialised it.
    let arc = Arc::new(cfg);
    if kevy::config_replace(arc.clone()).is_err() {
        kevy::config_init(arc);
    }
}

#[test]
fn eval_cross_slot_rejected_under_cluster() {
    install_cluster_enabled();
    let mut store = Store::new();
    // `foo` and `bar` hash to different CRC16 slots — they don't
    // share a `{hashtag}`, so they'll collide and trigger CROSSSLOT.
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[
            b"EVAL",
            b"return KEYS[1] .. KEYS[2]",
            b"2",
            b"foo",
            b"bar",
        ]),
    );
    assert!(
        reply.starts_with(b"-CROSSSLOT "),
        "expected -CROSSSLOT, got: {:?}",
        String::from_utf8_lossy(&reply)
    );
}

#[test]
fn eval_same_slot_via_hashtag_accepted_under_cluster() {
    install_cluster_enabled();
    let mut store = Store::new();
    // `{tag}` shared between both keys → both hash to the same slot.
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[
            b"EVAL",
            b"return KEYS[1] .. '+' .. KEYS[2]",
            b"2",
            b"{user:42}:profile",
            b"{user:42}:settings",
        ]),
    );
    assert_eq!(reply, b"$36\r\n{user:42}:profile+{user:42}:settings\r\n");
}

#[test]
fn evalsha_cross_slot_rejected_under_cluster() {
    install_cluster_enabled();
    let mut store = Store::new();
    let load_reply = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"LOAD", b"return KEYS[1] .. KEYS[2]"]),
    );
    let sha_hex = load_reply[5..45].to_vec();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVALSHA", &sha_hex, b"2", b"foo", b"bar"]),
    );
    assert!(reply.starts_with(b"-CROSSSLOT "));
}

#[test]
fn eval_single_key_accepted_under_cluster() {
    install_cluster_enabled();
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL", b"return KEYS[1]", b"1", b"foo"]),
    );
    assert_eq!(reply, b"$3\r\nfoo\r\n");
}

#[test]
fn eval_zero_keys_accepted_under_cluster() {
    install_cluster_enabled();
    let mut store = Store::new();
    let reply = kevy::dispatch(&mut store, &argv(&[b"EVAL", b"return 1", b"0"]));
    assert_eq!(reply, b":1\r\n");
}

// `eval_multi_key_accepted_when_cluster_off` lives in `lua_eval.rs`
// where the default-Config invariant holds. Mixing it into this
// binary causes a cargo parallel-test race with the other tests
// here that flip cluster mode on.
