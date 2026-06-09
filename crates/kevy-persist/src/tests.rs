use super::*;
use std::fs::OpenOptions;
use std::time::Duration;

fn temp_file(name: &str) -> std::path::PathBuf {
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

fn cmd(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

#[test]
fn aof_append_and_replay() {
    let path = temp_file("aof");
    {
        let mut aof = Aof::open(&path, Fsync::Always).unwrap();
        aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
        aof.append(&cmd(&[b"INCR", b"a"])).unwrap();
        aof.append(&cmd(&[b"SET", b"b", b"hello world"])).unwrap();
    }
    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], cmd(&[b"SET", b"a", b"1"]));
    assert_eq!(got[1], cmd(&[b"INCR", b"a"]));
    assert_eq!(got[2], cmd(&[b"SET", b"b", b"hello world"]));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aof_group_commit_defers_then_flushes() {
    // appendfsync=always group commit: inside begin_group/end_group the
    // appends buffer (one fsync per batch), and end_group makes them all
    // durable BEFORE the caller sends replies. Guards the durable-before-
    // reply contract the reactor relies on.
    let path = temp_file("aofgroup");
    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    aof.begin_group();
    aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
    aof.append(&cmd(&[b"SET", b"b", b"2"])).unwrap();
    aof.append(&cmd(&[b"SET", b"c", b"3"])).unwrap();
    // Mid-group, before end_group: the batch is still buffered, not on disk.
    let mut mid: Vec<Argv> = Vec::new();
    replay_aof(&path, |a| mid.push(a)).unwrap();
    assert!(mid.is_empty(), "group commit must defer until end_group, saw {}", mid.len());
    // end_group does the single fsync for the whole batch.
    aof.end_group().unwrap();
    let mut after: Vec<Argv> = Vec::new();
    replay_aof(&path, |a| after.push(a)).unwrap();
    assert_eq!(after, vec![
        cmd(&[b"SET", b"a", b"1"]),
        cmd(&[b"SET", b"b", b"2"]),
        cmd(&[b"SET", b"c", b"3"]),
    ]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aof_truncated_tail_ignored() {
    let path = temp_file("aoftail");
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
    }
    // Simulate a crash mid-append: a partial frame at the end.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"*2\r\n$3\r\nSET\r\n$5\r\nhal").unwrap(); // truncated
    drop(f);

    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got, vec![cmd(&[b"SET", b"a", b"1"])]); // only the complete frame
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aof_truncate_clears() {
    let path = temp_file("aoftrunc");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
    aof.truncate().unwrap();
    aof.append(&cmd(&[b"SET", b"b", b"2"])).unwrap();
    drop(aof);

    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got, vec![cmd(&[b"SET", b"b", b"2"])]); // pre-truncate write gone
    let _ = std::fs::remove_file(&path);
}

#[test]
fn replay_missing_file_is_ok() {
    let path = temp_file("nofile");
    let mut n = 0;
    replay_aof(&path, |_| n += 1).unwrap();
    assert_eq!(n, 0);
}

/// The mailrs prod incident shape: SSH stderr ("Warning: Permanently
/// added 't02.golia.jp' …") got redirected into the AOF by a deploy
/// pipeline. RESP has an *inline* form (space-tokenized for raw-typed
/// PING / DEBUG), so the junk does parse into commands — but kevy
/// must NOT panic, and the dispatcher above will reject the bogus
/// verbs at -ERR level. This test pins the lower-level guarantee:
/// replay returns Ok and processes every byte without crash, even
/// when the bytes are clearly not anything we ever wrote.
#[test]
fn replay_aof_with_ssh_stderr_head_does_not_panic() {
    use std::io::Write;
    let path = temp_file("ssh_warning_head");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(
        b"Warning: Permanently added 't02.golia.jp' (ED25519) to the list of known hosts.\r\n",
    ).unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n").unwrap();
    drop(f);
    let mut n = 0;
    replay_aof(&path, |_| n += 1).expect("replay must not panic on junk input");
    // The SSH stderr line and the trailing SET both produce "commands"
    // at the parse layer (inline + multibulk). The summary line on
    // stderr will show this count — operations notices it's wrong.
    assert!(n >= 2, "saw at least the inline junk + the SET, got {n}");
    let _ = std::fs::remove_file(&path);
}

/// A *real* malformed RESP frame (`*` header with non-numeric count)
/// triggers the parser's Err path — and exercises the "WARN with
/// hex preview" branch of replay_aof. The clean prefix replays;
/// the corrupt frame + everything after is dropped; the function
/// still returns Ok.
/// New AOFs created by `Aof::open` carry the v1.2.0 `KEVYAOF1\n`
/// magic header. `replay_aof` strips it before parsing RESP.
#[test]
fn fresh_aof_has_magic_header_and_replays_cleanly() {
    use std::io::Read;
    let path = temp_aof("magic-fresh");
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
            .unwrap();
    }
    // Inspect bytes on disk: first 9 must be the magic.
    let mut f = std::fs::File::open(&path).unwrap();
    let mut buf = [0u8; 9];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"KEVYAOF1\n");
    // Replay: should see exactly one command, not the magic.
    let mut seen: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| seen.push(args)).unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].get(0).unwrap(), b"SET");
    let _ = std::fs::remove_file(&path);
}

/// Pre-1.2.0 AOFs ("legacy bare-RESP", no magic header) still replay
/// — `replay_aof` only consumes the magic if it sees it. Backward-
/// compat is non-negotiable for the install base.
#[test]
fn legacy_aof_without_magic_still_replays() {
    use std::io::Write;
    let path = temp_aof("magic-legacy");
    // Build a bare-RESP AOF by hand (no magic prefix). Mirrors what a
    // 1.0/1.1 server would have written.
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n").unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nx\r\n$1\r\ny\r\n").unwrap();
    drop(f);
    let mut seen: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| seen.push(args)).unwrap();
    assert_eq!(seen.len(), 2);
    let _ = std::fs::remove_file(&path);
}

/// `Aof::truncate` rewrites the file to just the magic header — so
/// post-truncate replays still identify the file as kevy-managed.
#[test]
fn truncate_preserves_magic_header() {
    use std::io::Read;
    let path = temp_aof("magic-truncate");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
        .unwrap();
    aof.truncate().unwrap();
    assert_eq!(aof.size_bytes(), 9);
    drop(aof);
    let mut f = std::fs::File::open(&path).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"KEVYAOF1\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn replay_aof_with_real_corrupt_frame_keeps_prefix() {
    use std::io::Write;
    let path = temp_file("real_corrupt_mid");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n").unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n").unwrap();
    // Multi-bulk start byte (`*`) with non-numeric length → Err path.
    f.write_all(b"*BAD\r\n").unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nc\r\n$1\r\n3\r\n").unwrap();
    drop(f);
    let mut n = 0;
    replay_aof(&path, |_| n += 1).expect("replay must not panic on corrupt frame");
    assert_eq!(n, 2, "prefix replays; corrupt frame stops the loop; tail dropped");
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

// ───────────── AOF rewrite (Wave 2 #3) ─────────────

/// Tiny dispatch helper for AOF-rewrite roundtrip tests: turn the
/// canonical mutating verbs the rewriter emits back into Store mutations.
/// Mirrors a subset of `kevy::dispatch` — enough for the verbs
/// `dump_store_to_aof` actually emits.
fn apply_for_test(store: &mut Store, args: &Argv) {
    let verb = args[0].to_ascii_uppercase();
    match verb.as_slice() {
        b"SET" => {
            store.set(&args[1], args[2].to_vec(), None, false, false);
        }
        b"HSET" => {
            let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            let mut i = 2;
            while i + 1 < args.len() {
                pairs.push((args[i].to_vec(), args[i + 1].to_vec()));
                i += 2;
            }
            store.hset(&args[1], &pairs).unwrap();
        }
        b"RPUSH" => {
            let items: Vec<Vec<u8>> = args.iter().skip(2).map(|a| a.to_vec()).collect();
            store.rpush(&args[1], &items).unwrap();
        }
        b"SADD" => {
            let members: Vec<Vec<u8>> = args.iter().skip(2).map(|a| a.to_vec()).collect();
            store.sadd(&args[1], &members).unwrap();
        }
        b"ZADD" => {
            let mut pairs: Vec<(f64, Vec<u8>)> = Vec::new();
            let mut i = 2;
            while i + 1 < args.len() {
                let score: f64 = std::str::from_utf8(&args[i]).unwrap().parse().unwrap();
                pairs.push((score, args[i + 1].to_vec()));
                i += 2;
            }
            store.zadd(&args[1], &pairs).unwrap();
        }
        b"PEXPIRE" => {
            let ms: u64 = std::str::from_utf8(&args[2]).unwrap().parse().unwrap();
            store.expire(&args[1], Duration::from_millis(ms));
        }
        b"PEXPIREAT" => {
            // The rewrite now emits absolute deadlines (INC-2026-06-09 fix).
            let deadline: u64 = std::str::from_utf8(&args[2]).unwrap().parse().unwrap();
            store.expire_at_unix_ms(&args[1], deadline);
        }
        other => panic!("unexpected verb in AOF rewrite: {:?}", String::from_utf8_lossy(other)),
    }
}

fn temp_aof(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("kevy-{name}-{uniq}.aof"));
    p
}

#[test]
fn rewrite_reconstructs_full_keyspace() {
    let path = temp_aof("rewrite-all");

    let mut src = Store::new();
    src.set(b"str", b"hello".to_vec(), None, false, false);
    src.set(b"binary", vec![0u8, 1, 2, 255], None, false, false);
    src.hset(b"hash", &[(b"f1".to_vec(), b"v1".to_vec()), (b"f2".to_vec(), b"v2".to_vec())])
        .unwrap();
    src.rpush(b"list", &[b"i1".to_vec(), b"i2".to_vec(), b"i3".to_vec()])
        .unwrap();
    src.sadd(b"set", &[b"m1".to_vec(), b"m2".to_vec()]).unwrap();
    src.zadd(b"zset", &[(1.5, b"a".to_vec()), (2.5, b"b".to_vec())])
        .unwrap();
    src.set(
        b"ttl",
        b"x".to_vec(),
        Some(Duration::from_secs(3600)),
        false,
        false,
    );

    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    let stats = aof.rewrite_from(&src).unwrap();
    assert_eq!(stats.keys, 7);
    assert!(stats.bytes > 0);
    assert_eq!(aof.size_bytes(), stats.bytes);
    assert_eq!(aof.size_at_last_rewrite(), stats.bytes);
    assert_eq!(aof.rewrites_total(), 1);
    drop(aof);

    // Replay into a fresh store; both should match.
    let mut dst = Store::new();
    replay_aof(&path, |args| apply_for_test(&mut dst, &args)).unwrap();
    assert_eq!(dst.dbsize(), 7);
    assert_eq!(dst.get(b"str").unwrap(), Some(&b"hello"[..]));
    assert_eq!(dst.get(b"binary").unwrap(), Some(&[0u8, 1, 2, 255][..]));
    assert_eq!(dst.hget(b"hash", b"f1").unwrap(), Some(&b"v1"[..]));
    assert_eq!(dst.hget(b"hash", b"f2").unwrap(), Some(&b"v2"[..]));
    assert_eq!(dst.llen(b"list").unwrap(), 3);
    assert_eq!(dst.scard(b"set").unwrap(), 2);
    assert_eq!(dst.zcard(b"zset").unwrap(), 2);
    assert!(dst.pttl(b"ttl") > 3_500_000); // TTL survived
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rewrite_replaces_old_log_atomically() {
    let path = temp_aof("rewrite-swap");

    // Step 1: a stale AOF with many entries (simulating long-running
    // history). After rewrite the new AOF must NOT carry these.
    {
        let mut aof = Aof::open(&path, Fsync::Always).unwrap();
        for i in 0..50 {
            let k = format!("k{i}");
            let argv = Argv::from(vec![b"SET".to_vec(), k.into_bytes(), b"v".to_vec()]);
            aof.append(&argv).unwrap();
        }
    }
    let big_size = std::fs::metadata(&path).unwrap().len();
    assert!(big_size > 0);

    // Step 2: in-memory state is small (only 2 keys).
    let mut store = Store::new();
    store.set(b"only", b"value".to_vec(), None, false, false);
    store.set(b"second", b"v2".to_vec(), None, false, false);
    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    let stats = aof.rewrite_from(&store).unwrap();
    assert_eq!(stats.keys, 2);
    let new_size = std::fs::metadata(&path).unwrap().len();
    assert!(new_size < big_size, "rewrite should shrink: {new_size} vs {big_size}");

    // Step 3: appending after rewrite lands in the new file.
    aof.append(&Argv::from(vec![b"SET".to_vec(), b"third".to_vec(), b"v".to_vec()]))
        .unwrap();
    drop(aof);

    let mut dst = Store::new();
    replay_aof(&path, |args| apply_for_test(&mut dst, &args)).unwrap();
    assert_eq!(dst.dbsize(), 3, "rewrite + append should yield 3 keys");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn append_bumps_size_estimate() {
    let path = temp_aof("size-est");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    // Fresh AOF carries the 9-byte AOF_MAGIC header (v1.2.0+).
    let base = aof.size_bytes();
    aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
        .unwrap();
    let after_one = aof.size_bytes();
    assert!(after_one > base);
    aof.append(&Argv::from(vec![b"SET".to_vec(), b"k2".to_vec(), b"v".to_vec()]))
        .unwrap();
    assert!(aof.size_bytes() > after_one);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rewrite_resets_size_anchor() {
    let path = temp_aof("size-anchor");
    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    for _ in 0..10 {
        aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
            .unwrap();
    }
    assert!(aof.size_bytes() > aof.size_at_last_rewrite());
    let store = Store::new();
    let stats = aof.rewrite_from(&store).unwrap();
    // empty store ⇒ empty rewrite (just the 9-byte AOF_MAGIC header).
    assert_eq!(stats.keys, 0);
    // dump_store_to_aof prefixes the file with AOF_MAGIC (9 bytes).
    assert_eq!(aof.size_bytes(), 9);
    assert_eq!(aof.size_at_last_rewrite(), 9);
    assert_eq!(aof.rewrites_total(), 1);
    let _ = std::fs::remove_file(&path);
}
