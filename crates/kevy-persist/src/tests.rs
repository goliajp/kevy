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
