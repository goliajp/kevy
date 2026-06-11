use super::*;
use std::time::Duration;

pub(crate) fn temp_file(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("kevy-{name}-{uniq}.rdb"));
    p
}

#[test]
fn snapshot_round_trip() {
    let path = temp_file("rt");

    let mut src = Store::new();
    src.set(b"plain", b"value".to_vec(), None, false, false);
    src.set(b"empty", Vec::new(), None, false, false);
    src.set(b"binary", vec![0u8, 1, 2, 255, 254], None, false, false);
    src.set(
        b"withttl",
        b"soon".to_vec(),
        Some(Duration::from_secs(100)),
        false,
        false,
    );

    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();

    assert_eq!(dst.dbsize(), 4);
    assert_eq!(dst.get(b"plain").unwrap(), Some(&b"value"[..]));
    assert_eq!(dst.get(b"empty").unwrap(), Some(&b""[..]));
    assert_eq!(
        dst.get(b"binary").unwrap(),
        Some(&[0u8, 1, 2, 255, 254][..])
    );
    assert_eq!(dst.get(b"withttl").unwrap(), Some(&b"soon"[..]));
    // TTL survived (stored as an absolute Unix-ms deadline, v3 format).
    assert!(dst.pttl(b"withttl") > 90_000);

    let _ = std::fs::remove_file(&path);
}

/// INC-2026-06-09 regression: a snapshot stores the **absolute** deadline, so
/// time elapsed between save and load is subtracted from the restored TTL.
/// The pre-fix v2 format stored remaining-ms and re-anchored on load, so the
/// TTL would read back ~unchanged regardless of the gap.
#[test]
fn snapshot_ttl_is_absolute_across_delay() {
    let path = temp_file("ttl-abs");
    let mut src = Store::new();
    src.set(b"k", b"v".to_vec(), Some(Duration::from_secs(100)), false, false);
    save_snapshot(&src, &path).unwrap();

    std::thread::sleep(Duration::from_millis(1500));

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    let pttl = dst.pttl(b"k");
    // ~1.5 s of the 100 s elapsed while "down": deadline preserved => < 99 s.
    assert!(
        (0..99_000).contains(&pttl),
        "PTTL after delayed load = {pttl} ms; absolute deadline not preserved"
    );
    assert!(pttl > 90_000, "PTTL {pttl} implausibly low");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn bad_magic_is_rejected() {
    let path = temp_file("bad");
    std::fs::write(&path, b"NOTKEVY!....").unwrap();
    let mut dst = Store::new();
    assert!(load_snapshot(&mut dst, &path).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn expired_keys_are_not_saved() {
    let path = temp_file("exp");
    let mut src = Store::new();
    src.set(b"live", b"1".to_vec(), None, false, false);
    src.set(
        b"dead",
        b"2".to_vec(),
        Some(Duration::from_millis(1)),
        false,
        false,
    );
    std::thread::sleep(Duration::from_millis(8));

    save_snapshot(&src, &path).unwrap();
    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();

    assert_eq!(dst.dbsize(), 1);
    assert_eq!(dst.get(b"live").unwrap(), Some(&b"1"[..]));
    assert_eq!(dst.get(b"dead").unwrap(), None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hash_snapshot_round_trip() {
    let path = temp_file("hashrt");
    let mut src = Store::new();
    src.hset(
        b"h",
        &[
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"two".to_vec()),
        ],
    )
    .unwrap();
    src.set(b"s", b"str".to_vec(), None, false, false);
    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    assert_eq!(dst.type_of(b"h"), "hash");
    assert_eq!(dst.hget(b"h", b"a").unwrap(), Some(&b"1"[..]));
    assert_eq!(dst.hget(b"h", b"b").unwrap(), Some(&b"two"[..]));
    assert_eq!(dst.hlen(b"h"), Ok(2));
    assert_eq!(dst.get(b"s").unwrap(), Some(&b"str"[..]));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn list_snapshot_round_trip() {
    let path = temp_file("listrt");
    let mut src = Store::new();
    src.rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]).unwrap();
    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    assert_eq!(dst.type_of(b"l"), "list");
    assert_eq!(dst.llen(b"l"), Ok(3));
    assert_eq!(dst.lrange(b"l", 0, -1).unwrap(), vec![
        b"a".to_vec(), b"b".to_vec(), b"c".to_vec()
    ]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn set_snapshot_round_trip() {
    let path = temp_file("setrt");
    let mut src = Store::new();
    src.sadd(b"s", &[b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]).unwrap();
    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    assert_eq!(dst.type_of(b"s"), "set");
    assert_eq!(dst.scard(b"s"), Ok(3));
    let mut members = dst.smembers(b"s").unwrap();
    members.sort();
    assert_eq!(members, vec![b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn zset_snapshot_round_trip() {
    let path = temp_file("zsetrt");
    let mut src = Store::new();
    src.zadd(b"z", &[(1.0, b"a".to_vec()), (2.0, b"b".to_vec()), (0.5, b"c".to_vec())]).unwrap();
    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    assert_eq!(dst.type_of(b"z"), "zset");
    assert_eq!(dst.zcard(b"z"), Ok(3));
    // Ascending score order: c(0.5), a(1.0), b(2.0)
    let range = dst.zrange(b"z", 0, -1).unwrap();
    assert_eq!(range, vec![
        (b"c".to_vec(), 0.5),
        (b"a".to_vec(), 1.0),
        (b"b".to_vec(), 2.0),
    ]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn all_types_snapshot_round_trip() {
    let path = temp_file("allrt");
    let mut src = Store::new();
    src.set(b"str", b"hello".to_vec(), None, false, false);
    src.hset(b"hash", &[(b"f".to_vec(), b"v".to_vec())]).unwrap();
    src.rpush(b"list", &[b"i".to_vec()]).unwrap();
    src.sadd(b"set", &[b"m".to_vec()]).unwrap();
    src.zadd(b"zset", &[(1.0, b"k".to_vec())]).unwrap();
    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    assert_eq!(dst.dbsize(), 5);
    assert_eq!(dst.type_of(b"str"), "string");
    assert_eq!(dst.type_of(b"hash"), "hash");
    assert_eq!(dst.type_of(b"list"), "list");
    assert_eq!(dst.type_of(b"set"), "set");
    assert_eq!(dst.type_of(b"zset"), "zset");
    let _ = std::fs::remove_file(&path);
}

// ───────────── stream consumer groups (snapshot v4, 2026-06-11) ─────────────

use kevy_store::{GroupCreateMode, ReadGroupId, StreamId, XAddIdSpec};

/// Entries 1-1/2-1/3-1; group `g`; `c1` holds 1-1+2-1 (t=1000), `c2`
/// holds 3-1 (t=2000); 2-1 XDEL'd → its PEL row is a tombstone.
fn grouped_stream_store() -> Store {
    let mut src = Store::new();
    for ms in [1u64, 2, 3] {
        src.xadd(
            b"st",
            XAddIdSpec::Explicit(StreamId { ms, seq: 1 }),
            vec![(b"f".to_vec(), b"v".to_vec())],
            false,
            0,
        )
        .unwrap();
    }
    src.xgroup_create(b"st", b"g", GroupCreateMode::AtId(StreamId::MIN), false)
        .unwrap();
    src.xreadgroup(b"st", b"g", b"c1", ReadGroupId::New, Some(2), false, 1000)
        .unwrap();
    src.xreadgroup(b"st", b"g", b"c2", ReadGroupId::New, None, false, 2000)
        .unwrap();
    src.xdel(b"st", &[StreamId { ms: 2, seq: 1 }]).unwrap();
    src
}

#[test]
fn stream_groups_snapshot_round_trip() {
    let path = temp_file("groups");
    let src = grouped_stream_store();
    save_snapshot(&src, &path).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();

    let view = dst.stream_view(b"st").unwrap().unwrap();
    assert_eq!(view.length(), 2);
    assert_eq!(view.last_id(), StreamId { ms: 3, seq: 1 });
    assert_eq!(view.entries_added(), 3);
    assert_eq!(view.max_deleted_id(), StreamId { ms: 2, seq: 1 });
    let g = view.group(b"g").expect("group must survive the snapshot");
    assert_eq!(g.last_delivered_id(), StreamId { ms: 3, seq: 1 });
    // Snapshot is the full-fidelity path: the 2-1 tombstone survives.
    assert_eq!(g.pending_count(), 3);
    let p2 = g.pel.get(&StreamId { ms: 2, seq: 1 }).unwrap();
    assert_eq!(
        (p2.consumer.as_slice(), p2.delivery_time_ms, p2.delivery_count),
        (&b"c1"[..], 1000, 1)
    );
    let mut consumers: Vec<(Vec<u8>, u64, usize)> = g
        .consumers_iter()
        .map(|(n, c)| (n.to_vec(), c.last_seen_ms(), c.pending_count()))
        .collect();
    consumers.sort();
    assert_eq!(
        consumers,
        vec![(b"c1".to_vec(), 1000, 2), (b"c2".to_vec(), 2000, 1)]
    );
    let _ = std::fs::remove_file(&path);
}

/// A v3 file (pre-groups format) must still load — entries + scalars
/// present, group map empty. Hand-encoded bytes pin the v3 layout.
#[test]
fn v3_snapshot_without_groups_still_loads() {
    let path = temp_file("v3compat");
    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(b"KEVYSNAP");
    b.push(3); // version 3: no group section after the entries block
    b.push(6); // OP_STREAM
    b.push(0); // no TTL
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(b"st");
    for v in [1u64, 1, 0, 0, 1] {
        // last(1,1), max_deleted(0,0), entries_added=1
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&1u32.to_le_bytes()); // 1 entry
    b.extend_from_slice(&1u64.to_le_bytes()); // ms
    b.extend_from_slice(&1u64.to_le_bytes()); // seq
    b.extend_from_slice(&1u32.to_le_bytes()); // 1 field
    b.extend_from_slice(&1u32.to_le_bytes());
    b.push(b'f');
    b.extend_from_slice(&1u32.to_le_bytes());
    b.push(b'v');
    b.push(0); // OP_EOF
    std::fs::write(&path, &b).unwrap();

    let mut dst = Store::new();
    load_snapshot(&mut dst, &path).unwrap();
    let view = dst.stream_view(b"st").unwrap().unwrap();
    assert_eq!(view.length(), 1);
    assert_eq!(view.last_id(), StreamId { ms: 1, seq: 1 });
    assert_eq!(view.group_count(), 0);
    let _ = std::fs::remove_file(&path);
}

// ─────────────── SnapshotView serialization (COW E-3) ───────────────

fn populated_store() -> Store {
    let mut s = Store::new();
    s.set(b"s1", b"plain".to_vec(), None, false, false);
    s.set(b"s2", vec![b'x'; 100], None, false, false); // heap str
    s.hset(b"h", &[(b"f".to_vec(), b"v".to_vec())]).unwrap();
    s.rpush(b"l", &[b"a".to_vec(), b"b".to_vec()]).unwrap();
    s.sadd(b"set", &[b"m1".to_vec(), b"m2".to_vec()]).unwrap();
    s.zadd(b"z", &[(1.5, b"one".to_vec())]).unwrap();
    s
}

/// A frozen view serializes byte-identically to the live store it froze
/// (no-TTL data: TTL deadlines are stamped at write time and would
/// legitimately differ between two writes).
#[test]
fn view_snapshot_bytes_match_store_snapshot() {
    let s = populated_store();
    let view = s.collect_snapshot();
    let dir = std::env::temp_dir();
    let p_store = dir.join(format!("kevy-e3-store-{}.rdb", std::process::id()));
    let p_view = dir.join(format!("kevy-e3-view-{}.rdb", std::process::id()));
    save_snapshot(&s, &p_store).unwrap();
    save_snapshot(&view, &p_view).unwrap();
    assert_eq!(std::fs::read(&p_store).unwrap(), std::fs::read(&p_view).unwrap());
    let _ = std::fs::remove_file(&p_store);
    let _ = std::fs::remove_file(&p_view);
}

/// View-serialized AOF replays into an equivalent store, and reflects the
/// collect instant — not mutations that landed during/after serialization.
#[test]
fn view_aof_round_trips_at_the_collect_instant() {
    let mut s = populated_store();
    let view = s.collect_snapshot();
    // Post-collect mutations must not appear in the dump.
    s.set(b"s1", b"mutated".to_vec(), None, false, false);
    s.hset(b"h", &[(b"f2".to_vec(), b"late".to_vec())]).unwrap();

    let p = std::env::temp_dir().join(format!("kevy-e3-aof-{}.aof", std::process::id()));
    let (keys, bytes) = dump_aof(&p, &view).unwrap();
    assert_eq!(keys, 6);
    assert!(bytes > 0);

    let mut restored = Store::new();
    replay_aof(&p, |args| {
        crate::tests_rewrite::apply_for_test(&mut restored, &args);
    })
    .unwrap();
    assert_eq!(restored.get(b"s1").unwrap(), Some(b"plain".as_slice()));
    assert_eq!(restored.hget(b"h", b"f2").unwrap(), None);
    assert_eq!(restored.hget(b"h", b"f").unwrap(), Some(b"v".as_slice()));
    let _ = std::fs::remove_file(&p);
}
