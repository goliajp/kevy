//! Snapshot-view (COW serialization) tests — split from `tests.rs` to
//! keep both under the 500-LOC house rule.

use crate::*;
use std::time::Duration;

// ──────────────── collect_snapshot (COW-serialization E-2) ────────────────

fn view_get(view: &crate::SnapshotView, key: &[u8]) -> Option<Value> {
    let mut found = None;
    view.each(|k, v, _| {
        if k == key {
            found = Some(v.clone());
        }
    });
    found
}

/// The view is a frozen instant: post-collect overwrites, deletions, and
/// inserts are all invisible to it.
#[test]
fn snapshot_view_is_point_in_time_for_strings() {
    let mut s = Store::new();
    s.set(b"a", b"old".to_vec(), None, false, false);
    s.set(b"gone", b"x".to_vec(), None, false, false);

    let view = s.collect_snapshot();
    s.set(b"a", b"new".to_vec(), None, false, false);
    s.del(&[b"gone".to_vec()]);
    s.set(b"later", b"y".to_vec(), None, false, false);

    assert_eq!(view.len(), 2);
    match view_get(&view, b"a") {
        Some(Value::Str(v)) => assert_eq!(v.as_slice(), b"old"),
        other => panic!("expected frozen Str, got {:?}", other.map(|v| v.type_name())),
    }
    assert!(view_get(&view, b"gone").is_some(), "deleted key must stay in the view");
    assert!(view_get(&view, b"later").is_none(), "post-collect insert leaked in");
    // The live store sees its own mutations.
    assert_eq!(s.get(b"a").unwrap(), Some(b"new".as_slice()));
}

/// Collection mutation after collect must copy-on-write: the store's hash
/// changes, the view's stays at the collect-time contents.
#[test]
fn snapshot_view_collections_are_cow() {
    let mut s = Store::new();
    s.hset(b"h", &[(b"f".to_vec(), b"v1".to_vec())]).unwrap();

    let view = s.collect_snapshot();
    s.hset(b"h", &[(b"f".to_vec(), b"v2".to_vec()), (b"g".to_vec(), b"w".to_vec())]).unwrap();

    match view_get(&view, b"h") {
        Some(Value::Hash(h)) => {
            assert_eq!(h.len(), 1, "view hash gained post-collect fields");
            assert_eq!(h.get(b"f".as_slice()).map(|v| v.as_slice()), Some(b"v1".as_slice()));
        }
        other => panic!("expected frozen Hash, got {:?}", other.map(|v| v.type_name())),
    }
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(b"v2".as_slice()));
    assert_eq!(s.hget(b"h", b"g").unwrap(), Some(b"w".as_slice()));
}

/// Deleting the only live owner must not free what the view still holds —
/// the view keeps the payload alive via its strong ref.
#[test]
fn snapshot_view_outlives_deletion_of_collections() {
    let mut s = Store::new();
    for i in 0..100u32 {
        s.hset(b"big", &[(format!("f{i}").into_bytes(), vec![b'x'; 64])]).unwrap();
    }
    let view = s.collect_snapshot();
    s.del(&[b"big".to_vec()]);
    drop(s);
    match view_get(&view, b"big") {
        Some(Value::Hash(h)) => assert_eq!(h.len(), 100),
        _ => panic!("view lost the deleted hash"),
    }
}

/// TTLs resolve at collect time; expired-but-unreaped entries are skipped.
#[test]
fn snapshot_view_ttl_semantics() {
    let mut s = Store::new();
    s.set(b"t", b"v".to_vec(), Some(Duration::from_secs(100)), false, false);
    s.set(b"dead", b"v".to_vec(), Some(Duration::from_millis(1)), false, false);
    std::thread::sleep(Duration::from_millis(5));

    let view = s.collect_snapshot();
    assert_eq!(view.len(), 1, "expired entry leaked into the view");
    let mut ttl_seen = None;
    view.each(|k, _, ttl| {
        if k == b"t" {
            ttl_seen = ttl;
        }
    });
    let ttl = ttl_seen.expect("ttl key missing");
    assert!(ttl > 90_000 && ttl <= 100_000, "ttl {ttl} not in collect-time range");
}

/// The view crosses threads (Send) and serializes concurrently with writes.
#[test]
fn snapshot_view_serializes_on_another_thread() {
    let mut s = Store::new();
    for i in 0..1000u32 {
        s.set(format!("k{i}").as_bytes(), format!("v{i}").into_bytes(), None, false, false);
    }
    let view = s.collect_snapshot();
    let handle = std::thread::spawn(move || {
        let mut n = 0usize;
        view.each(|_, _, _| n += 1);
        n
    });
    for i in 0..1000u32 {
        s.set(format!("k{i}").as_bytes(), b"mutated".to_vec(), None, false, false);
    }
    assert_eq!(handle.join().unwrap(), 1000);
}

/// Not a perf gate — a sanity measurement that the collect pause is in the
/// O(ns/entry) class, not O(serialized bytes). Prints the figure; asserts
/// only a generous ceiling so CI noise can't flake it.
#[test]
fn collect_pause_is_shallow() {
    let mut s = Store::new();
    for i in 0..1_000_000u32 {
        s.set(format!("key:{i:07}").as_bytes(), b"v0123456789".to_vec(), None, false, false);
    }
    // A few collections to amortize the first-touch page faults.
    let mut best = u128::MAX;
    for _ in 0..3 {
        let t0 = std::time::Instant::now();
        let view = s.collect_snapshot();
        let dt = t0.elapsed().as_micros();
        assert_eq!(view.len(), 1_000_000);
        best = best.min(dt);
    }
    eprintln!("collect_snapshot: 1M string keys in {best} us ({:.1} ns/entry)", best as f64 / 1000.0);
    // Generous ceiling: even a debug build on a loaded CI box clears this;
    // a regression to deep-copy semantics (O(serialized bytes)) would not.
    assert!(best < 2_000_000, "collect took {best} us — deep copy regression?");
}

/// Collections are refcount-bumped, not walked: collecting a store whose
/// few keys hold huge hashes must cost the same class as tiny ones.
#[test]
fn collect_pause_is_independent_of_collection_size() {
    let mut s = Store::new();
    for k in 0..10u32 {
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..100_000u32)
            .map(|i| (format!("f{i}").into_bytes(), b"valueval".to_vec()))
            .collect();
        s.hset(format!("big{k}").as_bytes(), &pairs).unwrap();
    }
    let t0 = std::time::Instant::now();
    let view = s.collect_snapshot();
    let dt = t0.elapsed().as_micros();
    assert_eq!(view.len(), 10);
    eprintln!("collect_snapshot: 10 x 100k-field hashes in {dt} us");
    // 1M nested fields; a deep walk would take milliseconds even in
    // release. Shallow = 10 entries, microseconds.
    assert!(dt < 50_000, "collect walked into collections? {dt} us");
}
