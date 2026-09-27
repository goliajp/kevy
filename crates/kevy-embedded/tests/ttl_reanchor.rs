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

fn dispatch(s: &Store, parts: &[&[u8]]) -> Vec<u8> {
    let argv: Vec<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    out
}

/// Every byte of every AOF file under `dir`.
fn aof_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut all = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                all.extend(std::fs::read(&p).unwrap());
            }
        }
    }
    all
}

/// SET with NX/XX and a TTL is one operation, logged in the server's shape:
/// the value's own frame carries the TTL, then PEXPIREAT pins the deadline,
/// so no crash point leaves the value without a TTL.
#[test]
fn conditional_set_with_ttl_is_one_frame_and_survives_replay() {
    let dir = kevy_tmpdir::TmpDir::new("ttl-reanchor-setnx");
    let ttl = reopened_ttl_after(dir.path(), |s| {
        assert_eq!(dispatch(s, &[b"SET", b"k", b"v", b"NX", b"EX", b"100"]), b"+OK\r\n");
        assert_eq!(dispatch(s, &[b"SET", b"k", b"w", b"NX", b"EX", b"100"]), b"$-1\r\n");
        assert_eq!(dispatch(s, &[b"SET", b"k", b"v", b"XX", b"PX", b"100000"]), b"+OK\r\n");
        assert_eq!(dispatch(s, &[b"SET", b"gone", b"v", b"XX", b"EX", b"5"]), b"$-1\r\n");
    });
    assert!(ttl > 0, "key survived: {ttl}");
    assert!(ttl <= 100_000 - 1_000, "conditional SET re-anchored on replay: {ttl}ms");
    let aof = aof_bytes(dir.path());
    let count = |pat: &[u8]| aof.windows(pat.len()).filter(|w| *w == pat).count();
    // the frame is the SET as it was run, so the TTL option is EX or PX
    let ttl_frames = count(b"$2\r\nEX\r\n") + count(b"$2\r\nPX\r\n");
    assert_eq!(ttl_frames, 2, "each applied SET carries its TTL in its own frame");
    assert_eq!(count(b"PEXPIREAT"), 2, "and each is pinned to an absolute deadline");
    assert_eq!(count(b"$4\r\ngone"), 0, "a vetoed SET is not logged");
}

/// The EXPIRE replies come from the one store call that acted, not from an
/// existence check taken under a different lock.
#[test]
fn expire_replies_describe_what_happened() {
    let s = Store::open(Config::default()).expect("open");
    assert_eq!(dispatch(&s, &[b"EXPIRE", b"none", b"10"]), b":0\r\n");
    assert_eq!(dispatch(&s, &[b"EXPIRE", b"none", b"0"]), b":0\r\n");
    assert_eq!(dispatch(&s, &[b"PEXPIREAT", b"none", b"1"]), b":0\r\n");
    s.set(b"k", b"v").unwrap();
    assert_eq!(dispatch(&s, &[b"EXPIRE", b"k", b"10"]), b":1\r\n");
    assert_eq!(dispatch(&s, &[b"EXPIRE", b"k", b"-1"]), b":1\r\n");
    assert_eq!(s.get(b"k").unwrap(), None);
}

/// A closed store refuses a conditional SET before touching the keyspace.
#[test]
fn conditional_set_on_a_closed_store_is_refused() {
    let s = Store::open(Config::default()).expect("open");
    s.shutdown().unwrap();
    assert!(s.set_with_ttl(b"k", b"v", Duration::from_secs(1)).is_err());
    let reply = dispatch(&s, &[b"SET", b"k", b"v", b"NX", b"EX", b"1"]);
    assert!(reply.starts_with(b"-"), "{:?}", String::from_utf8_lossy(&reply));
}
