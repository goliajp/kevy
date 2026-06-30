//! Tests for the multi-shard atomic transaction in `ops_atomic_all.rs`
//! (kevy-embedded 1.13.0).

use crate::Config;
use crate::store::Store;

fn s_with_shards(n: usize) -> Store {
    Store::open(Config::default().with_shards(n).with_ttl_reaper_manual()).unwrap()
}

// ---- multi-shard SET visibility -----------------------------------------

#[test]
fn atomic_all_sees_each_keys_writes_across_shards() {
    let s = s_with_shards(4);
    s.atomic_all_shards(|tx| {
        tx.set(b"k0", b"v0");
        tx.set(b"k1", b"v1");
        tx.set(b"k2", b"v2");
        tx.set(b"k3", b"v3");
        tx.set(b"k4", b"v4");
        tx.set(b"k5", b"v5");
        tx.set(b"k6", b"v6");
        tx.set(b"k7", b"v7");
        // Reads inside the closure see prior writes regardless of
        // which shard owns each key.
        assert_eq!(tx.get(b"k0")?, Some(b"v0".to_vec()));
        assert_eq!(tx.get(b"k7")?, Some(b"v7".to_vec()));
        Ok::<(), std::io::Error>(())
    }).unwrap();
    for i in 0..8 {
        let key = format!("k{i}");
        let val = format!("v{i}");
        assert_eq!(s.get(key.as_bytes()).unwrap(), Some(val.into_bytes()));
    }
}

// ---- multi-shard read-modify-write loop ---------------------------------

#[test]
fn atomic_all_rmw_across_shards() {
    let s = s_with_shards(4);
    // Pre-seed counters in two different shards.
    s.set(b"counter:a", b"10").unwrap();
    s.set(b"counter:b", b"20").unwrap();
    s.atomic_all_shards(|tx| {
        let a = parse(&tx.get(b"counter:a")?.unwrap_or_default());
        let b = parse(&tx.get(b"counter:b")?.unwrap_or_default());
        tx.set(b"counter:a", (a + b).to_string().as_bytes());
        tx.set(b"counter:b", (b - a).to_string().as_bytes());
        Ok::<(), std::io::Error>(())
    }).unwrap();
    assert_eq!(s.get(b"counter:a").unwrap(), Some(b"30".to_vec()));
    assert_eq!(s.get(b"counter:b").unwrap(), Some(b"10".to_vec()));
}

fn parse(bytes: &[u8]) -> i64 {
    std::str::from_utf8(bytes).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

// ---- multi-shard hash + zset --------------------------------------------

#[test]
fn atomic_all_hash_and_zset_ops() {
    let s = s_with_shards(4);
    let final_score: f64 = s
        .atomic_all_shards(|tx| {
            tx.hset(b"h1", &[(b"f", b"v")])?;
            tx.hincrby(b"h2", b"n", 5)?;
            tx.zadd(b"z1", &[(1.0, b"m")])?;
            tx.zincrby(b"z1", 2.5, b"m")
        })
        .unwrap();
    assert!((final_score - 3.5).abs() < 1e-9);
    assert_eq!(s.hget(b"h1", b"f").unwrap(), Some(b"v".to_vec()));
    assert_eq!(s.hget(b"h2", b"n").unwrap(), Some(b"5".to_vec()));
    assert_eq!(s.zscore(b"z1", b"m").unwrap(), Some(3.5));
}

// ---- multi-shard error propagation --------------------------------------

#[test]
fn atomic_all_error_propagates() {
    let s = s_with_shards(2);
    s.set(b"k", b"not-an-int").unwrap();
    let r: Result<(), _> = s.atomic_all_shards(|tx| {
        tx.incr(b"k")?; // WrongType -> NotInteger
        Ok(())
    });
    assert!(r.is_err());
}

// ---- single-shard config still works ------------------------------------

#[test]
fn atomic_all_works_on_single_shard_config() {
    let s = s_with_shards(1);
    s.atomic_all_shards(|tx| {
        tx.set(b"a", b"1");
        tx.set(b"b", b"2");
        Ok::<(), std::io::Error>(())
    }).unwrap();
    assert_eq!(s.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(s.get(b"b").unwrap(), Some(b"2".to_vec()));
}
