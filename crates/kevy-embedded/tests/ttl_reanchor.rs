//! A relative TTL frame in the AOF re-anchors on every replay — the
//! key gets its full TTL back at each restart, which is the incident
//! class the absolute-PEXPIREAT logging exists to prevent. These pin
//! every TTL-writing surface to the absolute form.

#![cfg(feature = "persist")]

use std::time::Duration;

use kevy_embedded::{Config, Store};

fn reopened_ttl_after(dir: &std::path::Path, setup: impl FnOnce(&Store)) -> i64 {
    let store = Store::open(Config::default().with_persist(dir)).expect("open");
    setup(&store);
    drop(store);
    std::thread::sleep(Duration::from_millis(1500));
    let store = Store::open(Config::default().with_persist(dir)).expect("reopen");
    store.ttl_ms(b"k")
}

/// The write path the consumer report exercised: a plain expire.
#[test]
fn expire_survives_replay_without_reanchoring() {
    let dir = kevy_tmpdir::TmpDir::new("ttl-reanchor-expire");
    let ttl = reopened_ttl_after(dir.path(), |s| {
        s.set(b"k", b"v").unwrap();
        assert!(s.expire(b"k", Duration::from_secs(100)).unwrap());
    });
    assert!(ttl > 0, "key survived: {ttl}");
    assert!(ttl <= 100_000 - 1_000, "EXPIRE re-anchored on replay: {ttl}ms of 100000ms");
}

/// GETEX updates the TTL atomically — and must persist it the same
/// absolute way EXPIRE does, not as a relative frame that re-anchors.
#[test]
fn getex_survives_replay_without_reanchoring() {
    let dir = kevy_tmpdir::TmpDir::new("ttl-reanchor-getex");
    let ttl = reopened_ttl_after(dir.path(), |s| {
        s.set(b"k", b"v").unwrap();
        assert!(s.getex(b"k", Duration::from_secs(100)).unwrap().is_some());
    });
    assert!(ttl > 0, "key survived: {ttl}");
    assert!(ttl <= 100_000 - 1_000, "GETEX re-anchored on replay: {ttl}ms of 100000ms");
}

/// And the one-call form.
#[test]
fn set_with_ttl_survives_replay_without_reanchoring() {
    let dir = kevy_tmpdir::TmpDir::new("ttl-reanchor-setttl");
    let ttl = reopened_ttl_after(dir.path(), |s| {
        s.set_with_ttl(b"k", b"v", Duration::from_secs(100)).unwrap();
    });
    assert!(ttl > 0, "key survived: {ttl}");
    assert!(ttl <= 100_000 - 1_000, "set_with_ttl re-anchored on replay: {ttl}ms of 100000ms");
}
