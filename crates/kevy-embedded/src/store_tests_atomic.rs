//! Tests for `atomic` + `pipeline` in `ops_atomic.rs` + `ops_pipeline.rs`
//! (kevy-embedded 1.10.0).

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- atomic --------------------------------------------------------------

#[test]
fn atomic_read_modify_write_loop() {
    let s = s();
    s.set(b"counter", b"10").unwrap();
    let result: i64 = s.atomic(|tx| {
        let cur = tx
            .get(b"counter")?
            .and_then(|v| std::str::from_utf8(&v).ok().map(|x| x.parse::<i64>().ok().unwrap_or(0)))
            .unwrap_or(0);
        let next = cur * 2 + 1;
        tx.set(b"counter", next.to_string().as_bytes());
        Ok(next)
    }).unwrap();
    assert_eq!(result, 21);
    assert_eq!(s.get(b"counter").unwrap(), Some(b"21".to_vec()));
}

#[test]
fn atomic_multi_op_commits_together() {
    let s = s();
    s.atomic(|tx| {
        tx.set(b"a", b"1");
        tx.set(b"b", b"2");
        tx.hset(b"h", &[(b"f", b"v")])?;
        tx.zadd(b"z", &[(1.0, b"m")])?;
        Ok::<(), std::io::Error>(())
    }).unwrap();
    assert_eq!(s.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(s.get(b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(1.0));
}

#[test]
fn atomic_incr_visible_in_closure() {
    let s = s();
    let final_value: i64 = s.atomic(|tx| {
        tx.incr(b"k")?;
        tx.incr(b"k")?;
        tx.incr_by(b"k", 8)
    }).unwrap();
    assert_eq!(final_value, 10);
}

#[test]
fn atomic_error_propagates() {
    let s = s();
    s.set(b"k", b"not-a-number").unwrap();
    let r: Result<(), _> = s.atomic(|tx| {
        tx.incr(b"k")?; // WrongType -> propagates
        Ok(())
    });
    assert!(r.is_err());
}

#[test]
fn atomic_hash_field_increment() {
    let s = s();
    let final_n: i64 = s.atomic(|tx| {
        tx.hset(b"counters", &[(b"n", b"5")])?;
        tx.hincrby(b"counters", b"n", 7)?;
        tx.hincrby(b"counters", b"n", -2)
    }).unwrap();
    assert_eq!(final_n, 10);
    assert_eq!(s.hget(b"counters", b"n").unwrap(), Some(b"10".to_vec()));
}

#[test]
fn atomic_zset_incr_visible() {
    let s = s();
    let score: f64 = s.atomic(|tx| {
        tx.zadd(b"z", &[(0.0, b"m")])?;
        tx.zincrby(b"z", 1.5, b"m")?;
        tx.zincrby(b"z", 2.5, b"m")
    }).unwrap();
    assert!((score - 4.0).abs() < 1e-9);
}

// ---- pipeline ------------------------------------------------------------

#[test]
fn pipeline_applies_in_order() {
    let s = s();
    s.pipeline()
        .set(b"a", b"1")
        .set(b"b", b"2")
        .hset(b"h", &[(b"f", b"v")])
        .zadd(b"z", &[(1.0, b"m")])
        .commit()
        .unwrap();
    assert_eq!(s.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(s.get(b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(1.0));
}

#[test]
fn pipeline_empty_commit_is_noop() {
    let s = s();
    s.pipeline().commit().unwrap();
    assert_eq!(s.dbsize(), 0);
}

#[test]
fn pipeline_len_and_is_empty() {
    let s = s();
    let p = s.pipeline().set(b"a", b"1").set(b"b", b"2");
    assert_eq!(p.len(), 2);
    assert!(!p.is_empty());
}

#[test]
fn pipeline_list_and_set_ops() {
    let s = s();
    s.pipeline()
        .rpush(b"l", &[b"a", b"b", b"c"])
        .sadd(b"s", &[b"x", b"y"])
        .srem(b"s", &[b"x"])
        .commit()
        .unwrap();
    assert_eq!(s.llen(b"l").unwrap(), 3);
    let mut got = s.smembers(b"s").unwrap();
    got.sort();
    assert_eq!(got, vec![b"y".to_vec()]);
}

#[test]
fn pipeline_incr_and_hincrby() {
    let s = s();
    s.pipeline()
        .incr(b"c")
        .incr(b"c")
        .incr_by(b"c", 8)
        .hset(b"h", &[(b"f", b"0")])
        .hincrby(b"h", b"f", 5)
        .commit()
        .unwrap();
    assert_eq!(s.get(b"c").unwrap(), Some(b"10".to_vec()));
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(b"5".to_vec()));
}

#[test]
fn pipeline_zincrby_then_zrem() {
    let s = s();
    s.pipeline()
        .zadd(b"z", &[(1.0, b"a"), (2.0, b"b")])
        .zincrby(b"z", 5.0, b"a")
        .zrem(b"z", &[b"b"])
        .commit()
        .unwrap();
    assert_eq!(s.zscore(b"z", b"a").unwrap(), Some(6.0));
    assert_eq!(s.zscore(b"z", b"b").unwrap(), None);
}

// ---- v2.1: AtomicCtx completeness (Pipeline write parity + reads) --------

/// The mailrs mark_seen / move_category shapes: hash write + zrem of
/// index zsets + reads for branch decisions, all inside ONE block.
#[test]
fn atomic_full_surface_write_and_read() {
    let s = s();
    s.zadd(b"idx:unread", &[(1.0, b"t1")]).unwrap();
    s.zadd(b"idx:cat:old", &[(1.0, b"t1")]).unwrap();
    s.hset(b"row", &[(b"unread" as &[u8], b"3" as &[u8])]).unwrap();

    s.atomic(|c| {
        // Branch decision under the lock:
        let cur = c.hgetall(b"row")?;
        assert_eq!(cur.len(), 1);
        assert!(c.hexists(b"row", b"unread")?);
        assert_eq!(c.hmget(b"row", &[b"unread", b"nope"])?[1], None);
        // mark_seen: row update + index removal, atomically.
        c.hset(b"row", &[(b"unread", b"0")])?;
        assert_eq!(c.zrem(b"idx:unread", &[b"t1"])?, 1);
        // move_category: zrem old + zadd new.
        assert_eq!(c.zrem(b"idx:cat:old", &[b"t1"])?, 1);
        c.zadd(b"idx:cat:new", &[(2.0, b"t1")])?;
        assert_eq!(c.zcard(b"idx:cat:new")?, 1);
        // Collections + keyspace writes:
        c.sadd(b"tags", &[b"a", b"b"])?;
        c.srem(b"tags", &[b"b"])?;
        c.lpush(b"log", &[b"e1"])?;
        c.rpush(b"log", &[b"e2"])?;
        c.hdel(b"row", &[b"gone"])?;
        assert_eq!(c.exists(&[b"row", b"absent"]), 1);
        // idx:cat:old auto-deleted when zrem emptied it (Redis
        // empty-collection semantics) — deleting it again is 0.
        assert_eq!(c.del(&[b"idx:cat:old"]), 0);
        c.set(b"tmp", b"1");
        assert_eq!(c.del(&[b"tmp"]), 1);
        Ok(())
    })
    .unwrap();

    assert_eq!(s.zcard(b"idx:unread").unwrap(), 0);
    assert_eq!(s.zscore(b"idx:cat:new", b"t1").unwrap(), Some(2.0));
    assert_eq!(s.smembers(b"s-none").unwrap().len(), 0);
    assert!(s.sismember(b"tags", b"a").unwrap());
    assert!(!s.sismember(b"tags", b"b").unwrap());
    assert_eq!(s.llen(b"log").unwrap(), 2);
    assert_eq!(s.exists(&[b"idx:cat:old"]).unwrap(), 0);
}

/// Same surface exists on the all-shards ctx (parity guard: the two
/// ctxs drifted before — zscore was missing from AtomicAllShards).
#[test]
fn atomic_all_shards_full_surface() {
    let s = s();
    s.atomic_all_shards(|c| {
        c.hset(b"row", &[(b"f", b"v")])?;
        assert!(c.hexists(b"row", b"f")?);
        assert_eq!(c.hgetall(b"row")?.len(), 1);
        assert_eq!(c.hmget(b"row", &[b"f"])?[0], Some(b"v".to_vec()));
        c.zadd(b"z", &[(1.5, b"m")])?;
        assert_eq!(c.zscore(b"z", b"m")?, Some(1.5));
        assert_eq!(c.zcard(b"z")?, 1);
        c.sadd(b"set", &[b"x"])?;
        c.srem(b"set", &[b"x"])?;
        c.lpush(b"l", &[b"a"])?;
        c.rpush(b"l", &[b"b"])?;
        c.zrem(b"z", &[b"m"])?;
        assert_eq!(c.exists(&[b"row"]), 1);
        c.hdel(b"row", &[b"f"])?; // empties the hash → key auto-deleted
        assert_eq!(c.exists(&[b"row"]), 0);
        c.set(b"tmp", b"1");
        assert_eq!(c.del(&[b"tmp"]), 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(s.zcard(b"z").unwrap(), 0);
    assert_eq!(s.llen(b"l").unwrap(), 2);
    assert_eq!(s.exists(&[b"row", b"tmp"]).unwrap(), 0);
}

/// Every new atomic write must survive a reopen (replay-arm guard —
/// the v2.0.21 lesson applied to the new surface).
#[test]
fn atomic_new_writes_survive_reopen() {
    use crate::config::AppendFsync;
    let dir = crate::store::tests::tmp_dir("atomic-v21-reopen");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always),
        )
        .unwrap();
        s.atomic(|c| {
            c.sadd(b"s", &[b"a", b"b"])?;
            c.srem(b"s", &[b"b"])?;
            c.lpush(b"l", &[b"x"])?;
            c.rpush(b"l", &[b"y"])?;
            c.zadd(b"z", &[(1.0, b"m"), (2.0, b"n")])?;
            c.zrem(b"z", &[b"n"])?;
            c.hset(b"h", &[(b"f", b"v"), (b"g", b"w")])?;
            c.hdel(b"h", &[b"g"])?;
            c.set(b"k", b"v");
            c.del(&[b"k"]);
            Ok(())
        })
        .unwrap();
    }
    let s2 = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert!(s2.sismember(b"s", b"a").unwrap());
    assert!(!s2.sismember(b"s", b"b").unwrap());
    assert_eq!(s2.llen(b"l").unwrap(), 2);
    assert_eq!(s2.zscore(b"z", b"m").unwrap(), Some(1.0));
    assert_eq!(s2.zscore(b"z", b"n").unwrap(), None);
    assert_eq!(s2.hget(b"h", b"f").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s2.hget(b"h", b"g").unwrap(), None);
    assert_eq!(s2.get(b"k").unwrap(), None);
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- v2.1: ZADD flags across embedded surfaces ---------------------------

#[test]
fn zadd_flags_facade_pipeline_atomic_and_reopen() {
    use crate::ZaddFlags;
    use crate::config::AppendFsync;
    let gt = ZaddFlags { gt: true, ..ZaddFlags::default() };
    let dir = crate::store::tests::tmp_dir("zadd-flags-reopen");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always),
        )
        .unwrap();
        // Facade: GT heal — stale write vetoed, newer applied.
        s.zadd(b"z", &[(5.0, b"m")]).unwrap();
        assert_eq!(s.zadd_flags(b"z", &[(3.0, b"m")], gt).unwrap().changed, 0);
        assert_eq!(s.zadd_flags(b"z", &[(7.0, b"m")], gt).unwrap().changed, 1);
        // INCR form: veto returns None and writes nothing.
        assert_eq!(s.zadd_incr(b"z", -1.0, b"m", gt).unwrap(), None);
        assert_eq!(s.zadd_incr(b"z", 2.0, b"m", gt).unwrap(), Some(9.0));
        // Pipeline surface.
        s.pipeline().zadd_flags(b"z", &[(1.0, b"m"), (4.0, b"p")], gt).commit().unwrap();
        assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(9.0)); // vetoed
        assert_eq!(s.zscore(b"z", b"p").unwrap(), Some(4.0)); // added
        // Atomic surfaces.
        s.atomic(|c| {
            let rep = c.zadd_flags(b"z", &[(2.0, b"m"), (11.0, b"m2")], gt)?;
            assert_eq!((rep.added, rep.changed), (1, 1));
            Ok(())
        })
        .unwrap();
        s.atomic_all_shards(|c| {
            let rep = c.zadd_flags(b"z", &[(12.0, b"m")], gt)?;
            assert_eq!(rep.changed, 1);
            Ok(())
        })
        .unwrap();
        // Invalid combo rejected at the typed boundary.
        assert!(s
            .zadd_flags(b"z", &[(1.0, b"q")], ZaddFlags { nx: true, xx: true, ..ZaddFlags::default() })
            .is_err());
    }
    // Reopen: the logged *effects* replay to the exact same state.
    let s2 = Store::open(Config::default().with_persist(&dir).with_ttl_reaper_manual()).unwrap();
    assert_eq!(s2.zscore(b"z", b"m").unwrap(), Some(12.0));
    assert_eq!(s2.zscore(b"z", b"m2").unwrap(), Some(11.0));
    assert_eq!(s2.zscore(b"z", b"p").unwrap(), Some(4.0));
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}
