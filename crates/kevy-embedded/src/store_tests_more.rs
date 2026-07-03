//! Tests for the 12 more Redis ops in `ops_more.rs`
//! (kevy-embedded 1.11.0).

use crate::Config;
use crate::store::Store;

fn s() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

// ---- set extras ---------------------------------------------------------

#[test]
fn sismember_hit_and_miss() {
    let s = s();
    s.sadd(b"s", &[b"x", b"y"]).unwrap();
    assert!(s.sismember(b"s", b"x").unwrap());
    assert!(!s.sismember(b"s", b"z").unwrap());
    assert!(!s.sismember(b"absent", b"x").unwrap());
}

#[test]
fn spop_removes_and_returns() {
    let s = s();
    s.sadd(b"s", &[b"a", b"b", b"c"]).unwrap();
    let popped = s.spop(b"s", 2).unwrap();
    assert_eq!(popped.len(), 2);
    assert_eq!(s.scard(b"s").unwrap(), 1);
}

#[test]
fn srandmember_returns_without_remove() {
    let s = s();
    s.sadd(b"s", &[b"a", b"b", b"c"]).unwrap();
    let rand = s.srandmember(b"s", 2).unwrap();
    assert_eq!(rand.len(), 2);
    assert_eq!(s.scard(b"s").unwrap(), 3);
}

// ---- sorted set extras --------------------------------------------------

#[test]
fn zrank_ascending() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c")]).unwrap();
    assert_eq!(s.zrank(b"z", b"a").unwrap(), Some(0));
    assert_eq!(s.zrank(b"z", b"c").unwrap(), Some(2));
    assert_eq!(s.zrank(b"z", b"missing").unwrap(), None);
}

#[test]
fn zcount_inclusive_range() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")]).unwrap();
    assert_eq!(s.zcount(b"z", 2.0, 3.0).unwrap(), 2);
    assert_eq!(s.zcount(b"z", f64::NEG_INFINITY, f64::INFINITY).unwrap(), 4);
}

#[test]
fn zpopmin_removes_lowest() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c")]).unwrap();
    let popped = s.zpopmin(b"z", 2).unwrap();
    assert_eq!(popped, vec![(b"a".to_vec(), 1.0), (b"b".to_vec(), 2.0)]);
    assert_eq!(s.zcard(b"z").unwrap(), 1);
}

#[test]
fn zremrangebyrank_removes_top_k() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")]).unwrap();
    let removed = s.zremrangebyrank(b"z", 0, 1).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(s.zcard(b"z").unwrap(), 2);
}

#[test]
fn zremrangebyscore_removes_band() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c")]).unwrap();
    let removed = s.zremrangebyscore(b"z", 2.0, 3.0).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(s.zcard(b"z").unwrap(), 1);
}

#[test]
fn zrev_range_by_score_descending() {
    let s = s();
    s.zadd(b"z", &[(1.0, b"a"), (2.0, b"b"), (3.0, b"c"), (4.0, b"d")]).unwrap();
    let got = s.zrev_range_by_score(b"z", 3.0, 2.0).unwrap();
    assert_eq!(got, vec![(b"c".to_vec(), 3.0), (b"b".to_vec(), 2.0)]);
}

// ---- list extras --------------------------------------------------------

#[test]
fn lset_at_position() {
    let s = s();
    s.rpush(b"l", &[b"a", b"b", b"c"]).unwrap();
    s.lset(b"l", 1, b"B").unwrap();
    assert_eq!(s.lindex(b"l", 1).unwrap(), Some(b"B".to_vec()));
}

#[test]
fn lset_negative_index_from_tail() {
    let s = s();
    s.rpush(b"l", &[b"a", b"b", b"c"]).unwrap();
    s.lset(b"l", -1, b"C").unwrap();
    assert_eq!(s.lindex(b"l", -1).unwrap(), Some(b"C".to_vec()));
}

#[test]
fn ltrim_keeps_inclusive_range() {
    let s = s();
    s.rpush(b"l", &[b"a", b"b", b"c", b"d", b"e"]).unwrap();
    s.ltrim(b"l", 1, 3).unwrap();
    assert_eq!(
        s.lrange(b"l", 0, -1).unwrap(),
        vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
}

// ---- keyspace extras ----------------------------------------------------

#[test]
fn rename_moves_value() {
    let s = s();
    s.set(b"src", b"val").unwrap();
    assert!(s.rename(b"src", b"dst").unwrap());
    assert_eq!(s.get(b"src").unwrap(), None);
    assert_eq!(s.get(b"dst").unwrap(), Some(b"val".to_vec()));
}

#[test]
fn rename_no_such_src_errors() {
    let s = s();
    assert!(s.rename(b"absent", b"dst").is_err());
}

#[test]
fn renamenx_vetoes_when_dst_exists() {
    let s = s();
    s.set(b"src", b"v1").unwrap();
    s.set(b"dst", b"existing").unwrap();
    assert!(!s.renamenx(b"src", b"dst").unwrap());
    assert_eq!(s.get(b"dst").unwrap(), Some(b"existing".to_vec()));
}

#[test]
fn renamenx_succeeds_when_dst_absent() {
    let s = s();
    s.set(b"src", b"val").unwrap();
    assert!(s.renamenx(b"src", b"dst").unwrap());
    assert_eq!(s.get(b"dst").unwrap(), Some(b"val".to_vec()));
}

// ---- v2.2: zset algebra facades ------------------------------------------

#[test]
fn zset_algebra_store_forms_and_reopen() {
    use crate::ZAggregate;
    use crate::config::AppendFsync;
    let dir = crate::store::tests::tmp_dir("zalg-reopen");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always),
        )
        .unwrap();
        s.zadd(b"za", &[(1.0, b"x"), (2.0, b"y")]).unwrap();
        s.zadd(b"zb", &[(3.0, b"y"), (4.0, b"z")]).unwrap();
        s.sadd(b"set1", &[b"y", b"q"]).unwrap();

        // inter: zset ∩ zset
        assert_eq!(s.zinterstore(b"d1", &[b"za", b"zb"], None, ZAggregate::Sum).unwrap(), 1);
        assert_eq!(s.zscore(b"d1", b"y").unwrap(), Some(5.0));
        // weights + MAX
        assert_eq!(
            s.zinterstore(b"d2", &[b"za", b"zb"], Some(&[10.0, 1.0]), ZAggregate::Max).unwrap(),
            1
        );
        assert_eq!(s.zscore(b"d2", b"y").unwrap(), Some(20.0));
        // union with a plain set participating at score 1.0
        assert_eq!(s.zunionstore(b"d3", &[b"za", b"set1"], None, ZAggregate::Sum).unwrap(), 3);
        assert_eq!(s.zscore(b"d3", b"y").unwrap(), Some(3.0)); // 2.0 + 1.0
        assert_eq!(s.zscore(b"d3", b"q").unwrap(), Some(1.0));
        // diff + intercard
        assert_eq!(s.zdiffstore(b"d4", &[b"za", b"zb"]).unwrap(), 1);
        assert_eq!(s.zscore(b"d4", b"x").unwrap(), Some(1.0));
        assert_eq!(s.zintercard(&[b"za", b"zb"], 0).unwrap(), 1);
        // *STORE overwrites any old dst; empty result deletes dst
        s.set(b"d5", b"old").unwrap();
        assert_eq!(s.zinterstore(b"d5", &[b"za", b"missing"], None, ZAggregate::Sum).unwrap(), 0);
        assert_eq!(s.exists(&[b"d5"]).unwrap(), 0);
        // set-algebra store forms
        s.sadd(b"sa", &[b"a", b"b"]).unwrap();
        s.sadd(b"sb", &[b"b", b"c"]).unwrap();
        assert_eq!(s.sinterstore(b"sd1", &[b"sa", b"sb"]).unwrap(), 1);
        assert!(s.sismember(b"sd1", b"b").unwrap());
        assert_eq!(s.sunionstore(b"sd2", &[b"sa", b"sb"]).unwrap(), 3);
        assert_eq!(s.sdiffstore(b"sd3", &[b"sa", b"sb"]).unwrap(), 1);
        assert!(s.sismember(b"sd3", b"a").unwrap());
    }
    // effect-logged AOF replays to identical state
    let s2 = Store::open(Config::default().with_persist(&dir).with_ttl_reaper_manual()).unwrap();
    assert_eq!(s2.zscore(b"d1", b"y").unwrap(), Some(5.0));
    assert_eq!(s2.zscore(b"d2", b"y").unwrap(), Some(20.0));
    assert_eq!(s2.zscore(b"d3", b"q").unwrap(), Some(1.0));
    assert_eq!(s2.zscore(b"d4", b"x").unwrap(), Some(1.0));
    assert_eq!(s2.exists(&[b"d5"]).unwrap(), 0);
    assert!(s2.sismember(b"sd1", b"b").unwrap());
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}
