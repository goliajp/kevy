//! Reopen matrix: every write op the embedded facade can emit into the
//! AOF must survive a drop + reopen. Guards against the verb-coverage
//! drift class where an op's `commit_write` verb is missing from
//! `replay.rs` and the entry is silently skipped on load (data loss —
//! several long-shipped verbs were once found missing this way).

use super::tests::tmp_dir;
use crate::Store;
use crate::config::{AppendFsync, Config};

fn persist_cfg(dir: &std::path::Path) -> Config {
    Config::default()
        .with_persist(dir)
        .with_ttl_reaper_manual()
        .with_appendfsync(AppendFsync::Always)
}

fn reopen_cfg(dir: &std::path::Path) -> Config {
    Config::default().with_persist(dir).with_ttl_reaper_manual()
}

#[test]
fn replay_covers_bitmap_verbs() {
    let dir = tmp_dir("replay-bitmap");
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        s.setbit(b"bits", 7, 1).unwrap();
        s.setrange(b"range", 5, b"hello").unwrap();
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(s2.getbit(b"bits", 7).unwrap(), 1, "SETBIT lost on replay");
    assert_eq!(
        s2.getrange(b"range", 5, 9).unwrap(),
        b"hello".to_vec(),
        "SETRANGE lost on replay"
    );
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_covers_hash_verbs() {
    let dir = tmp_dir("replay-hash");
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        s.hsetnx(b"h", b"f", b"v").unwrap();
        s.hincrbyfloat(b"hf", b"score", 2.5).unwrap();
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(
        s2.hget(b"h", b"f").unwrap(),
        Some(b"v".to_vec()),
        "HSETNX lost on replay"
    );
    assert_eq!(
        s2.hget(b"hf", b"score").unwrap(),
        Some(b"2.5".to_vec()),
        "HINCRBYFLOAT lost on replay"
    );
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_covers_list_verbs() {
    let dir = tmp_dir("replay-list");
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        s.rpush(b"l", &[b"a", b"c"]).unwrap();
        s.linsert(b"l", true, b"c", b"b").unwrap();
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(
        s2.lrange(b"l", 0, -1).unwrap(),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
        "LINSERT lost on replay"
    );
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_covers_rename_verbs() {
    let dir = tmp_dir("replay-rename");
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        s.set(b"src", b"v1").unwrap();
        s.rename(b"src", b"dst").unwrap();
        s.set(b"src2", b"v2").unwrap();
        s.renamenx(b"src2", b"dst2").unwrap();
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(s2.get(b"src").unwrap(), None, "RENAME src survived replay");
    assert_eq!(
        s2.get(b"dst").unwrap(),
        Some(b"v1".to_vec()),
        "RENAME dst lost on replay"
    );
    assert_eq!(
        s2.get(b"dst2").unwrap(),
        Some(b"v2".to_vec()),
        "RENAMENX dst lost on replay"
    );
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_covers_zset_removal_verbs() {
    let dir = tmp_dir("replay-zset-rm");
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        let members: &[(f64, &[u8])] =
            &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")];
        s.zadd(b"zp", members).unwrap();
        s.zpopmin(b"zp", 1).unwrap(); // pops "a"
        s.zadd(b"zr", members).unwrap();
        s.zremrangebyrank(b"zr", 0, 0).unwrap(); // removes "a"
        s.zadd(b"zs", members).unwrap();
        s.zremrangebyscore(b"zs", 3.5, 5.0).unwrap(); // removes "d"
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(s2.zscore(b"zp", b"a").unwrap(), None, "ZPOPMIN lost on replay");
    assert_eq!(s2.zcard(b"zp").unwrap(), 3);
    assert_eq!(
        s2.zscore(b"zr", b"a").unwrap(),
        None,
        "ZREMRANGEBYRANK lost on replay"
    );
    assert_eq!(s2.zcard(b"zr").unwrap(), 3);
    assert_eq!(
        s2.zscore(b"zs", b"d").unwrap(),
        None,
        "ZREMRANGEBYSCORE lost on replay"
    );
    assert_eq!(s2.zcard(b"zs").unwrap(), 3);
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn copy_survives_reopen() {
    let dir = tmp_dir("replay-copy");
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        s.set(b"src", b"payload").unwrap();
        assert!(s.copy(b"src", b"dst", false).unwrap());
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(
        s2.get(b"dst").unwrap(),
        Some(b"payload".to_vec()),
        "COPY dst never reached the AOF"
    );
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spop_replay_removes_exactly_the_popped_members() {
    let dir = tmp_dir("replay-spop");
    let (popped, remaining_before): (Vec<Vec<u8>>, Vec<Vec<u8>>);
    {
        let s = Store::open(persist_cfg(&dir)).unwrap();
        s.sadd(b"s", &[b"a", b"b", b"c", b"d", b"e"]).unwrap();
        popped = s.spop(b"s", 2).unwrap();
        let mut m = s.smembers(b"s").unwrap();
        m.sort();
        remaining_before = m;
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    let mut remaining_after = s2.smembers(b"s").unwrap();
    remaining_after.sort();
    assert_eq!(
        remaining_after, remaining_before,
        "SPOP replay diverged: popped {popped:?} live but a different \
         member set after reopen"
    );
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Durability barrier: everysec store + `fsync_aof()` → writes
/// are on disk at the barrier (verified via reopen; the crash-window
/// semantics are the documented contract, exercised by chaos suites).
#[test]
fn fsync_aof_barrier_flushes_everysec() {
    let dir = tmp_dir("fsync-barrier");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::EverySec),
        )
        .unwrap();
        s.set(b"critical", b"v").unwrap();
        s.fsync_aof().unwrap();
        // Under `always` it's a no-op and must still be Ok.
        s.fsync_aof().unwrap();
    }
    let s2 = Store::open(reopen_cfg(&dir)).unwrap();
    assert_eq!(s2.get(b"critical").unwrap(), Some(b"v".to_vec()));
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The host-mediated pump pair: `dump_aof_buf` serializes the keyspace
/// as one AOF image (single magic header even when sharded), and
/// `apply_frame` replays frames into the right shard — including the
/// keyless FLUSHALL fan-out.
#[test]
fn apply_frame_and_dump_buf_roundtrip_sharded() {
    use kevy_persist::Argv;
    let frame = |parts: &[&[u8]]| {
        Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
    };

    let src = Store::open(Config::default().with_ttl_reaper_manual().with_shards(4)).unwrap();
    for i in 0..64 {
        let k = format!("key{i}");
        src.set(k.as_bytes(), b"v").unwrap();
    }
    let image = src.dump_aof_buf();
    assert!(image.starts_with(kevy_persist::AOF2_MAGIC));
    // Exactly one magic header: it must not recur past the start.
    assert!(
        !image[1..]
            .windows(kevy_persist::AOF2_MAGIC.len())
            .any(|w| w == kevy_persist::AOF2_MAGIC),
        "per-shard magic leaked into the concatenated image"
    );

    // Feed the image's records into a differently-sharded store.
    let dst = Store::open(Config::default().with_ttl_reaper_manual().with_shards(2)).unwrap();
    let mut pos = kevy_persist::AOF2_MAGIC.len();
    while let kevy_persist::RecordStep::Ok { payload, consumed } =
        kevy_persist::next_record(&image, pos)
    {
        let (args, used) = kevy_resp::parse_command(payload).unwrap().unwrap();
        assert_eq!(used, payload.len(), "record payload must be one command");
        dst.apply_frame(&args);
        pos += consumed;
    }
    assert_eq!(pos, image.len(), "image must parse to the last byte");
    assert_eq!(dst.dbsize(), 64);
    assert_eq!(dst.get(b"key42").unwrap(), Some(b"v".to_vec()));

    // Keyless frame fans out to every shard.
    dst.apply_frame(&frame(&[b"FLUSHALL"]));
    assert_eq!(dst.dbsize(), 0);

    // Keyed frames route by key regardless of case.
    dst.apply_frame(&frame(&[b"set", b"k", b"after"]));
    assert_eq!(dst.get(b"k").unwrap(), Some(b"after".to_vec()));
}
