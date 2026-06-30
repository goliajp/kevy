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
