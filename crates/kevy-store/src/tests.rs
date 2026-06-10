use super::*;
use std::time::Duration;

pub(crate) fn s(x: &str) -> Vec<u8> {
    x.as_bytes().to_vec()
}

#[test]
fn set_get_del_exists() {
    let mut st = Store::new();
    assert!(st.set(b"k", s("v"), None, false, false));
    assert_eq!(st.get(b"k"), Ok(Some(&b"v"[..])));
    assert_eq!(st.exists(&[s("k"), s("k"), s("nope")]), 2);
    assert_eq!(st.del(&[s("k"), s("nope")]), 1);
    assert_eq!(st.get(b"k"), Ok(None));
}

#[test]
fn set_nx_xx() {
    let mut st = Store::new();
    assert!(!st.set(b"k", s("v"), None, false, true));
    assert!(st.set(b"k", s("v"), None, true, false));
    assert!(!st.set(b"k", s("w"), None, true, false));
    assert_eq!(st.get(b"k"), Ok(Some(&b"v"[..])));
    assert!(st.set(b"k", s("w"), None, false, true));
    assert_eq!(st.get(b"k"), Ok(Some(&b"w"[..])));
}

#[test]
fn incr_paths() {
    let mut st = Store::new();
    assert_eq!(st.incr_by(b"n", 1), Ok(1));
    assert_eq!(st.incr_by(b"n", 41), Ok(42));
    assert_eq!(st.incr_by(b"n", -50), Ok(-8));
    st.set(b"s", s("abc"), None, false, false);
    assert_eq!(st.incr_by(b"s", 1), Err(StoreError::NotInteger));
    st.set(b"big", s(&i64::MAX.to_string()), None, false, false);
    assert_eq!(st.incr_by(b"big", 1), Err(StoreError::Overflow));
}

#[test]
fn ttl_expire_persist() {
    let mut st = Store::new();
    st.set(b"k", s("v"), None, false, false);
    assert_eq!(st.pttl(b"k"), -1);
    assert_eq!(st.pttl(b"missing"), -2);
    assert!(st.expire(b"k", Duration::from_secs(100)));
    assert!(st.pttl(b"k") > 99_000);
    assert!(st.persist(b"k"));
    assert_eq!(st.pttl(b"k"), -1);
    assert!(!st.persist(b"k"));
}

#[test]
fn lazy_expiry() {
    let mut st = Store::new();
    st.set(b"k", s("v"), Some(Duration::from_millis(1)), false, false);
    std::thread::sleep(Duration::from_millis(8));
    assert_eq!(st.get(b"k"), Ok(None));
    assert_eq!(st.exists(&[s("k")]), 0);
    assert_eq!(st.dbsize(), 0);
}

#[test]
fn append_strlen_type_flush() {
    let mut st = Store::new();
    assert_eq!(st.append(b"k", b"foo"), Ok(3));
    assert_eq!(st.append(b"k", b"bar"), Ok(6));
    assert_eq!(st.strlen(b"k"), Ok(6));
    assert_eq!(st.get(b"k"), Ok(Some(&b"foobar"[..])));
    assert_eq!(st.type_of(b"k"), "string");
    assert_eq!(st.type_of(b"missing"), "none");
    assert_eq!(st.dbsize(), 1);
    st.flush();
    assert_eq!(st.dbsize(), 0);
}

#[test]
fn hash_ops() {
    let mut st = Store::new();
    assert_eq!(st.hset(b"h", &[(s("a"), s("1")), (s("b"), s("2"))]), Ok(2));
    assert_eq!(st.hset(b"h", &[(s("a"), s("9"))]), Ok(0)); // update, not new
    assert_eq!(st.hget(b"h", b"a"), Ok(Some(&b"9"[..])));
    assert_eq!(st.hget(b"h", b"missing"), Ok(None));
    assert_eq!(st.hlen(b"h"), Ok(2));
    assert_eq!(st.hexists(b"h", b"b"), Ok(true));
    assert_eq!(st.type_of(b"h"), "hash");
    assert_eq!(st.hincrby(b"h", b"a", 1), Ok(10));
    assert!(!st.hsetnx(b"h", b"a", b"x").unwrap());
    assert!(st.hsetnx(b"h", b"c", b"3").unwrap());
    assert_eq!(
        st.hmget(b"h", &[s("a"), s("zzz")]),
        Ok(vec![Some(s("10")), None])
    );
    assert_eq!(st.hdel(b"h", &[s("a"), s("zzz")]), Ok(1));
    assert_eq!(st.hget(b"h", b"a"), Ok(None));
}

#[test]
fn wrong_type_errors() {
    let mut st = Store::new();
    st.hset(b"h", &[(s("f"), s("v"))]).unwrap();
    assert_eq!(st.get(b"h"), Err(StoreError::WrongType));
    assert_eq!(st.incr_by(b"h", 1), Err(StoreError::WrongType));
    st.set(b"s", s("v"), None, false, false);
    assert_eq!(st.hget(b"s", b"f"), Err(StoreError::WrongType));
    assert_eq!(
        st.hset(b"s", &[(s("f"), s("v"))]),
        Err(StoreError::WrongType)
    );
}

#[test]
fn list_ops() {
    let mut st = Store::new();
    assert_eq!(st.rpush(b"l", &[s("a"), s("b"), s("c")]), Ok(3));
    assert_eq!(st.lpush(b"l", &[s("x"), s("y")]), Ok(5)); // -> y x a b c
    assert_eq!(
        st.lrange(b"l", 0, -1),
        Ok(vec![s("y"), s("x"), s("a"), s("b"), s("c")])
    );
    assert_eq!(st.lindex(b"l", -1), Ok(Some(s("c"))));
    assert_eq!(st.lindex(b"l", 99), Ok(None));
    assert_eq!(st.llen(b"l"), Ok(5));
    assert_eq!(st.lpop(b"l", 1), Ok(vec![s("y")]));
    assert_eq!(st.rpop(b"l", 2), Ok(vec![s("c"), s("b")])); // -> x a
    assert_eq!(st.lrange(b"l", 0, -1), Ok(vec![s("x"), s("a")]));
    st.lset(b"l", 0, b"X").unwrap();
    assert_eq!(st.lindex(b"l", 0), Ok(Some(s("X"))));
    assert_eq!(st.lset(b"l", 9, b"z"), Err(StoreError::OutOfRange));
    assert_eq!(st.lset(b"missing", 0, b"z"), Err(StoreError::NoSuchKey));
    assert_eq!(st.type_of(b"l"), "list");
}

#[test]
fn list_lrem_ltrim_and_empty_delete() {
    let mut st = Store::new();
    st.rpush(b"l", &[s("a"), s("b"), s("a"), s("c"), s("a")])
        .unwrap();
    assert_eq!(st.lrem(b"l", 2, b"a"), Ok(2)); // remove first 2 'a' -> b c a
    assert_eq!(st.lrange(b"l", 0, -1), Ok(vec![s("b"), s("c"), s("a")]));
    st.ltrim(b"l", 1, 1).unwrap(); // keep only 'c'
    assert_eq!(st.lrange(b"l", 0, -1), Ok(vec![s("c")]));
    assert_eq!(st.lpop(b"l", 5), Ok(vec![s("c")]));
    assert_eq!(st.type_of(b"l"), "none"); // emptied list key is deleted
    assert_eq!(st.dbsize(), 0);
}

#[test]
fn list_wrong_type() {
    let mut st = Store::new();
    st.set(b"s", s("v"), None, false, false);
    assert_eq!(st.lpush(b"s", &[s("x")]), Err(StoreError::WrongType));
    st.rpush(b"l", &[s("a")]).unwrap();
    assert_eq!(st.get(b"l"), Err(StoreError::WrongType));
}

#[test]
fn list_wrong_type_on_read_path() {
    // list_ref WrongType branch — every read accessor returns WrongType when
    // the key holds a string. Drives the `_ => Err(WrongType)` arm in list_ref.
    let mut st = Store::new();
    st.set(b"s", s("v"), None, false, false);
    assert_eq!(st.lrange(b"s", 0, -1), Err(StoreError::WrongType));
    assert_eq!(st.llen(b"s"), Err(StoreError::WrongType));
    assert_eq!(st.lindex(b"s", 0), Err(StoreError::WrongType));
    // list_mut WrongType branch on the read-only path (`create=false`).
    assert_eq!(st.lpop(b"s", 1), Err(StoreError::WrongType));
    assert_eq!(st.rpop(b"s", 1), Err(StoreError::WrongType));
    assert_eq!(st.ltrim(b"s", 0, 0), Err(StoreError::WrongType));
    assert_eq!(st.lrem(b"s", 1, b"v"), Err(StoreError::WrongType));
    assert_eq!(st.lset(b"s", 0, b"v"), Err(StoreError::WrongType));
}

#[test]
fn list_empty_and_missing_key_paths() {
    // Missing-key paths: lpop/rpop return empty Vec without error; llen returns 0;
    // lindex/lrange return None/empty; lrem returns 0; ltrim is a no-op.
    let mut st = Store::new();
    assert_eq!(st.lpop(b"missing", 5), Ok(vec![]));
    assert_eq!(st.rpop(b"missing", 5), Ok(vec![]));
    assert_eq!(st.llen(b"missing"), Ok(0));
    assert_eq!(st.lindex(b"missing", 0), Ok(None));
    assert_eq!(st.lrange(b"missing", 0, -1), Ok(vec![]));
    assert_eq!(st.lrem(b"missing", 0, b"x"), Ok(0));
    assert!(st.ltrim(b"missing", 0, 0).is_ok());

    // pop_more_than_size: `None => break` arm — pop 5 from a 2-elt list, get 2.
    st.rpush(b"l", &[s("a"), s("b")]).unwrap();
    assert_eq!(st.lpop(b"l", 5), Ok(vec![s("a"), s("b")]));
    assert_eq!(st.type_of(b"l"), "none"); // emptied → key removed
}

#[test]
fn list_lrem_negative_count_and_lset_errors() {
    let mut st = Store::new();
    // LREM with negative count — drives the reverse-walk branch.
    st.rpush(b"l", &[s("a"), s("b"), s("a"), s("c"), s("a")])
        .unwrap();
    assert_eq!(st.lrem(b"l", -2, b"a"), Ok(2)); // remove last 2 'a' from tail
    assert_eq!(st.lrange(b"l", 0, -1), Ok(vec![s("a"), s("b"), s("c")]));

    // LSET error paths: NoSuchKey + OutOfRange.
    assert_eq!(st.lset(b"missing", 0, b"v"), Err(StoreError::NoSuchKey));
    assert_eq!(st.lset(b"l", 99, b"v"), Err(StoreError::OutOfRange));
    // Successful lset.
    assert!(st.lset(b"l", 1, b"B").is_ok());
    assert_eq!(st.lindex(b"l", 1), Ok(Some(s("B"))));

    // LTRIM that empties → key drops; LTRIM no-overlap range also empties.
    st.rpush(b"x", &[s("a"), s("b")]).unwrap();
    st.ltrim(b"x", 5, 10).unwrap(); // out-of-bounds → empties
    assert_eq!(st.type_of(b"x"), "none");
}

#[test]
fn set_ops() {
    let mut st = Store::new();
    assert_eq!(st.sadd(b"s", &[s("a"), s("b"), s("a")]), Ok(2)); // dedup
    assert_eq!(st.sadd(b"s", &[s("c")]), Ok(1));
    assert_eq!(st.scard(b"s"), Ok(3));
    assert_eq!(st.sismember(b"s", b"b"), Ok(true));
    assert_eq!(st.sismember(b"s", b"zzz"), Ok(false));
    let mut members = st.smembers(b"s").unwrap();
    members.sort();
    assert_eq!(members, vec![s("a"), s("b"), s("c")]);
    assert_eq!(st.type_of(b"s"), "set");
    assert_eq!(st.srem(b"s", &[s("a"), s("zzz")]), Ok(1));
    assert_eq!(st.scard(b"s"), Ok(2));
    // pop everything -> key deleted
    let popped = st.spop(b"s", 10).unwrap();
    assert_eq!(popped.len(), 2);
    assert_eq!(st.type_of(b"s"), "none");
}

#[test]
fn set_wrong_type() {
    let mut st = Store::new();
    st.set(b"str", s("v"), None, false, false);
    assert_eq!(st.sadd(b"str", &[s("x")]), Err(StoreError::WrongType));
}

#[test]
fn zset_ops() {
    let mut st = Store::new();
    assert_eq!(
        st.zadd(b"z", &[(2.0, s("b")), (1.0, s("a")), (3.0, s("c"))]),
        Ok(3)
    );
    assert_eq!(st.zadd(b"z", &[(5.0, s("a"))]), Ok(0)); // update, not new
    assert_eq!(st.zscore(b"z", b"a"), Ok(Some(5.0)));
    assert_eq!(st.zcard(b"z"), Ok(3));
    assert_eq!(st.type_of(b"z"), "zset");
    // order by score now: b(2) c(3) a(5)
    assert_eq!(
        st.zrange(b"z", 0, -1),
        Ok(vec![(s("b"), 2.0), (s("c"), 3.0), (s("a"), 5.0)])
    );
    assert_eq!(st.zrank(b"z", b"c"), Ok(Some(1)));
    assert_eq!(st.zrank(b"z", b"missing"), Ok(None));
    assert_eq!(st.zincrby(b"z", 1.0, b"b"), Ok(3.0)); // b -> 3, ties with c
    let mid = st
        .zrange_by_score(
            b"z",
            ScoreBound {
                value: 3.0,
                exclusive: false,
            },
            ScoreBound {
                value: 4.0,
                exclusive: false,
            },
        )
        .unwrap();
    assert_eq!(mid.len(), 2); // b(3) and c(3)
    assert_eq!(
        st.zcount(
            b"z",
            ScoreBound {
                value: f64::NEG_INFINITY,
                exclusive: false
            },
            ScoreBound {
                value: f64::INFINITY,
                exclusive: false
            }
        ),
        Ok(3)
    );
    assert_eq!(st.zrem(b"z", &[s("a"), s("zzz")]), Ok(1));
    assert_eq!(st.zcard(b"z"), Ok(2));
}

#[test]
fn zset_wrong_type_and_empty_delete() {
    let mut st = Store::new();
    st.set(b"s", s("v"), None, false, false);
    assert_eq!(st.zadd(b"s", &[(1.0, s("m"))]), Err(StoreError::WrongType));
    st.zadd(b"z", &[(1.0, s("only"))]).unwrap();
    assert_eq!(st.zrem(b"z", &[s("only")]), Ok(1));
    assert_eq!(st.type_of(b"z"), "none"); // emptied zset key deleted
}

#[test]
fn glob_matching() {
    assert!(glob_match(b"*", b"anything"));
    assert!(glob_match(b"h?llo", b"hello"));
    assert!(glob_match(b"h*o", b"hippo"));
    assert!(!glob_match(b"h*o", b"hippy"));
    assert!(glob_match(b"user:*", b"user:1000"));
    assert!(glob_match(b"key:[0-9]", b"key:5"));
    assert!(!glob_match(b"key:[0-9]", b"key:a"));
    assert!(glob_match(b"key:[^0-9]", b"key:a"));
    assert!(glob_match(b"a\\*b", b"a*b"));
    assert!(!glob_match(b"a\\*b", b"axb"));
}

#[test]
fn collect_keys_test() {
    let mut st = Store::new();
    st.set(b"user:1", s("a"), None, false, false);
    st.set(b"user:2", s("b"), None, false, false);
    st.set(b"post:1", s("c"), None, false, false);
    assert_eq!(st.collect_keys(None, None).len(), 3);
    let mut users = st.collect_keys(Some(b"user:*"), None);
    users.sort();
    assert_eq!(users, vec![s("user:1"), s("user:2")]);
    assert_eq!(st.collect_keys(None, Some(1)).len(), 1);
}

#[test]
fn hdel_removes_empty_hash() {
    let mut st = Store::new();
    st.hset(b"h", &[(s("a"), s("1"))]).unwrap();
    assert_eq!(st.hdel(b"h", &[s("a")]), Ok(1));
    assert_eq!(st.type_of(b"h"), "none"); // key gone when hash empties
    assert_eq!(st.dbsize(), 0);
}


// ───────────── WATCH version tracking ─────────────

#[test]
fn record_watch_starts_at_zero_and_is_idempotent() {
    let mut s = Store::new();
    // Never-written + never-watched → record sees no prior entry,
    // inserts 0, returns 0. Second record on the same key returns
    // the same 0 (no spurious bump).
    assert_eq!(s.record_watch(b"k"), 0);
    assert_eq!(s.record_watch(b"k"), 0);
    assert_eq!(s.key_version(b"k"), 0);
}

#[test]
fn bump_if_watched_only_touches_tracked_keys() {
    let mut s = Store::new();
    // Untracked key: bump is a no-op (no insert side-effect).
    s.bump_if_watched(b"never_watched");
    assert_eq!(s.key_version(b"never_watched"), 0);
    // After WATCH, bumps stick.
    assert_eq!(s.record_watch(b"k"), 0);
    s.bump_if_watched(b"k");
    assert_eq!(s.key_version(b"k"), 1);
    s.bump_if_watched(b"k");
    s.bump_if_watched(b"k");
    assert_eq!(s.key_version(b"k"), 3);
}

#[test]
fn bump_all_watched_invalidates_every_tracked_entry() {
    let mut s = Store::new();
    s.record_watch(b"k1");
    s.record_watch(b"k2");
    s.bump_if_watched(b"k1"); // version 1
    s.bump_all_watched(); // both → +1
    assert_eq!(s.key_version(b"k1"), 2);
    assert_eq!(s.key_version(b"k2"), 1);
    // Untracked key still 0.
    assert_eq!(s.key_version(b"untouched"), 0);
}
