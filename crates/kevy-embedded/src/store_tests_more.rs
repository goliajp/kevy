//! Tests for the 12 more Redis ops in `ops_more.rs`
//! (kevy-embedded 1.11.0).

use crate::Config;
use crate::FeedError;
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

// ---- v2.3: CDC feed (changes_since / changes_tail) ------------------------

#[test]
fn feed_consume_loop_and_prefix() {
    let s = Store::open(Config::default().with_ttl_reaper_manual().with_feed(0)).unwrap();
    assert_eq!(s.feed_shards(), 1);
    let (g, off) = s.changes_tail().unwrap();
    assert_eq!((g, off), (1, 0));

    s.set(b"user:1", b"a").unwrap();
    s.set(b"sess:1", b"b").unwrap();
    s.zadd(b"user:z", &[(1.0, b"m")]).unwrap();

    // full stream
    let batch = s.changes_since(1, 0, 100, &[]).unwrap();
    assert_eq!(batch.changes.len(), 3);
    assert_eq!(batch.changes[0].argv[0], b"SET".to_vec());
    assert_eq!(batch.next, (1, 3));
    // caught up
    let empty = s.changes_since(1, 3, 100, &[]).unwrap();
    assert!(empty.changes.is_empty());
    assert_eq!(empty.next, (1, 3));
    // prefix filter drops sess:, keeps user: (cursor unchanged by filter)
    let user = s.changes_since(1, 0, 100, &[b"user:"]).unwrap();
    assert_eq!(user.changes.len(), 2);
    assert_eq!(user.next, (1, 3));
    // future cursor rejected
    assert!(matches!(s.changes_since(1, 99, 10, &[]), Err(FeedError::Future)));
    // disabled store answers Disabled
    let off_store = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    assert!(matches!(off_store.changes_tail(), Err(FeedError::Disabled)));
}

#[test]
fn feed_flushall_bumps_generation() {
    let s = Store::open(Config::default().with_ttl_reaper_manual().with_feed(0)).unwrap();
    s.set(b"k", b"v").unwrap();
    s.flushall().unwrap();
    let (g, off) = s.changes_tail().unwrap();
    assert_eq!((g, off), (2, 0));
    match s.changes_since(1, 0, 10, &[]) {
        Err(FeedError::Resync { generation, tail }) => {
            assert_eq!(generation, 2);
            assert_eq!(tail, 0);
        }
        other => panic!("expected Resync, got {other:?}"),
    }
}

#[test]
fn feed_clean_reopen_continues_crash_bumps() {
    let dir = crate::store::tests::tmp_dir("feed-reopen");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual().with_feed(0),
        )
        .unwrap();
        s.set(b"a", b"1").unwrap();
        s.set(b"b", b"2").unwrap();
        assert_eq!(s.changes_tail().unwrap(), (1, 2));
    } // clean drop → marker written after AOF flush

    {
        let s2 = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual().with_feed(0),
        )
        .unwrap();
        // clean reopen: same generation, offset continues
        assert_eq!(s2.changes_tail().unwrap(), (1, 2));
        s2.set(b"c", b"3").unwrap();
        let batch = s2.changes_since(1, 2, 10, &[]).unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.changes[0].offset, 2);
    }

    // simulate crash: markers gone → next open bumps
    std::fs::remove_file(std::path::Path::new(&dir).join("feed-0.meta")).unwrap();
    let s3 = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual().with_feed(0),
    )
    .unwrap();
    assert_eq!(s3.changes_tail().unwrap(), (2, 0));
    drop(s3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn info_prefix_counts() {
    let s = s();
    for i in 0..15 {
        s.set(format!("ip:{i}").as_bytes(), b"v").unwrap();
    }
    s.set(b"zz:1", b"v").unwrap();
    s.set_with_ttl(b"ip:0", b"v", std::time::Duration::from_secs(100)).unwrap();
    let info = s.info_prefix(b"ip:");
    assert_eq!(info.keys, 15);
    assert_eq!(info.expires, 1);
    assert_eq!(s.info_prefix(b"none:").keys, 0);
}

// ---- v2.4: zpopmin_below ---------------------------------------------------

#[test]
fn zpopmin_below_pops_due_jobs_and_replays() {
    let dir = crate::store::tests::tmp_dir("zpb-reopen");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        s.zadd(b"jobs", &[(10.0, b"due1"), (20.0, b"due2"), (99.0, b"later")]).unwrap();
        // strictly-below semantics: 20.0 is NOT < 20.0
        let due = s.zpopmin_below(b"jobs", 20.0, 10).unwrap();
        assert_eq!(due, vec![(b"due1".to_vec(), 10.0)]);
        // pop the rest that are due before 100
        let due = s.zpopmin_below(b"jobs", 100.0, 1).unwrap(); // count cap
        assert_eq!(due[0].0, b"due2".to_vec());
        assert_eq!(s.zcard(b"jobs").unwrap(), 1);
        // empty/absent + wrongtype
        assert!(s.zpopmin_below(b"missing", 5.0, 3).unwrap().is_empty());
        s.set(b"str", b"v").unwrap();
        assert!(s.zpopmin_below(b"str", 5.0, 3).is_err());
    }
    // ZREM effect replays
    let s2 = Store::open(Config::default().with_persist(&dir).with_ttl_reaper_manual()).unwrap();
    assert_eq!(s2.zcard(b"jobs").unwrap(), 1);
    assert_eq!(s2.zscore(b"jobs", b"later").unwrap(), Some(99.0));
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- v2.4: blocking pops ---------------------------------------------------

#[test]
fn blpop_wakes_on_push_and_times_out() {
    use std::time::{Duration, Instant};
    let s = s();
    // immediate hit (no blocking)
    s.rpush(b"bq", &[b"ready"]).unwrap();
    let hit = s.blpop(&[b"bq"], Some(Duration::from_millis(50))).unwrap();
    assert_eq!(hit, Some((b"bq".to_vec(), b"ready".to_vec())));

    // timeout on empty
    let t0 = Instant::now();
    let miss = s.blpop(&[b"bq"], Some(Duration::from_millis(80))).unwrap();
    assert_eq!(miss, None);
    assert!(t0.elapsed() >= Duration::from_millis(75), "waited the timeout");

    // cross-thread wake: consumer parks, producer pushes after 60ms
    let consumer = s.clone();
    let h = std::thread::spawn(move || {
        consumer.blpop(&[b"bq1", b"bq2"], Some(Duration::from_secs(5))).unwrap()
    });
    std::thread::sleep(Duration::from_millis(60));
    s.rpush(b"bq2", &[b"pushed"]).unwrap();
    let got = h.join().unwrap();
    assert_eq!(got, Some((b"bq2".to_vec(), b"pushed".to_vec())));
}

#[test]
fn bzpopmin_and_brpop_block_variants() {
    use std::time::Duration;
    let s = s();
    let consumer = s.clone();
    let h = std::thread::spawn(move || {
        consumer.bzpopmin(&[b"bz"], Some(Duration::from_secs(5))).unwrap()
    });
    std::thread::sleep(Duration::from_millis(40));
    s.zadd(b"bz", &[(7.0, b"m")]).unwrap();
    assert_eq!(h.join().unwrap(), Some((b"bz".to_vec(), b"m".to_vec(), 7.0)));

    s.rpush(b"br", &[b"a", b"b"]).unwrap();
    let got = s.brpop(&[b"br"], None).unwrap();
    assert_eq!(got, Some((b"br".to_vec(), b"b".to_vec()))); // tail end
}

// ---- v2.4: public snapshot view --------------------------------------------

#[test]
fn snapshot_view_is_point_in_time_and_prefix_scoped() {
    let s = s();
    for i in 0..10 {
        s.set(format!("sv:{i}").as_bytes(), b"v").unwrap();
    }
    s.set(b"other:1", b"v").unwrap();
    s.set_with_ttl(b"sv:0", b"v", std::time::Duration::from_secs(500)).unwrap();

    let snap = s.snapshot();
    // mutations AFTER the freeze are invisible in the view
    s.set(b"sv:999", b"late").unwrap();
    s.del(&[b"sv:1"]).unwrap();

    let keys = snap.keys_prefix(b"sv:");
    assert_eq!(keys.len(), 10, "10 sv: keys at freeze time");
    assert!(keys.iter().any(|e| e.key == b"sv:1"), "deleted-after still in view");
    assert!(!keys.iter().any(|e| e.key == b"sv:999"), "added-after not in view");
    assert!(
        keys.iter().find(|e| e.key == b"sv:0").unwrap().ttl_ms.is_some(),
        "ttl carried"
    );
    assert_eq!(snap.keys_prefix(b"other:").len(), 1);
    assert_eq!(snap.len(), 11);

    // typed access via each_prefix
    let mut string_count = 0;
    snap.each_prefix(b"sv:", |_, v, _| {
        if matches!(v, kevy_store::Value::Str(_) | kevy_store::Value::Int(_) | kevy_store::Value::ArcBulk(_)) {
            string_count += 1;
        }
    });
    assert_eq!(string_count, 10);
}

// ---- v2.4: hash field TTLs (embedded matrix) --------------------------------

#[test]
fn hash_field_ttl_full_matrix_with_reopen() {
    use crate::HExpireCond;
    use crate::config::AppendFsync;
    let dir = crate::store::tests::tmp_dir("hfttl-reopen");
    let far = kevy_store::now_unix_ms() + 200_000;
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual()
                .with_appendfsync(AppendFsync::Always),
        )
        .unwrap();
        s.hset(b"h", &[(b"keep", b"1"), (b"ttl", b"2"), (b"soon", b"3")]).unwrap();
        // absolute deadline via relative facade
        let codes = s
            .hexpire(b"h", &[b"ttl", b"missing"], std::time::Duration::from_secs(200), HExpireCond::Always)
            .unwrap();
        assert_eq!(codes, vec![1, -2]);
        // immediate-past absolute → delete, code 2
        assert_eq!(
            s.hpexpire_at(b"h", &[b"soon"], 1, HExpireCond::Always).unwrap(),
            vec![2]
        );
        assert!(!s.hexists(b"h", b"soon").unwrap());
        // httl visible
        let ttls = s.httl(b"h", &[b"ttl", b"keep"]).unwrap();
        assert!(ttls[0] > 100_000);
        assert_eq!(ttls[1], -1);
        // persist round-trip for another field then re-set ttl
        s.hpexpire_at(b"h", &[b"keep"], far, HExpireCond::Always).unwrap();
        assert_eq!(s.hpersist(b"h", &[b"keep"]).unwrap(), vec![1]);
    }
    // AOF replay: ttl field still carries its deadline, keep does not
    {
        let s2 = Store::open(Config::default().with_persist(&dir).with_ttl_reaper_manual()).unwrap();
        let ttls = s2.httl(b"h", &[b"ttl", b"keep"]).unwrap();
        assert!(ttls[0] > 0, "deadline survived replay: {ttls:?}");
        assert_eq!(ttls[1], -1, "persisted field stays persisted");
        // snapshot path: SAVE then reopen from the dump
        s2.save_snapshot().unwrap();
    }
    {
        let s3 = Store::open(Config::default().with_persist(&dir).with_ttl_reaper_manual()).unwrap();
        let ttls = s3.httl(b"h", &[b"ttl"]).unwrap();
        assert!(ttls[0] > 0, "deadline survived snapshot round-trip: {ttls:?}");
        drop(s3);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- v2.5: embedded secondary indexes ---------------------------------------

#[test]
fn idx_create_query_maintain_reopen() {
    use crate::{IndexKind, IndexValue, IndexValType};
    let dir = crate::store::tests::tmp_dir("idx-reopen");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        for i in 0..30 {
            s.hset(
                format!("row:{i}").as_bytes(),
                &[(b"score", format!("{}", i * 10).as_bytes())],
            )
            .unwrap();
        }
        s.hset(b"row:bad", &[(b"score", b"junk")]).unwrap();
        // create builds synchronously from pre-existing rows
        s.idx_create(b"score_idx", b"row:", b"score", IndexValType::I64, IndexKind::Range)
            .unwrap();
        let (hits, next) = s
            .idx_query(
                b"score_idx",
                &IndexValue::I64(0),
                &IndexValue::I64(100),
                None,
                100,
            )
            .unwrap();
        assert_eq!(hits.len(), 11, "0..=100 step 10");
        assert!(next.is_none());
        assert_eq!(hits[0].0, b"row:0".to_vec());

        // live maintenance: update / delete / new row
        s.hset(b"row:0", &[(b"score", b"999")]).unwrap();
        s.del(&[b"row:1"]).unwrap();
        s.hset(b"row:new", &[(b"score", b"50")]).unwrap();
        assert_eq!(
            s.idx_count(b"score_idx", &IndexValue::I64(0), &IndexValue::I64(100)).unwrap(),
            10,
            "row:0 moved out, row:1 gone, row:new in"
        );
        // cursor pagination
        let (page1, cur) = s
            .idx_query(b"score_idx", &IndexValue::I64(0), &IndexValue::I64(100), None, 4)
            .unwrap();
        let cur = cur.expect("more");
        let (page2, _) = s
            .idx_query(b"score_idx", &IndexValue::I64(0), &IndexValue::I64(100), Some(&cur), 100)
            .unwrap();
        assert_eq!(page1.len() + page2.len(), 10);
        for (k, _) in &page1 {
            assert!(!page2.iter().any(|(k2, _)| k2 == k), "disjoint pages");
        }
        // stats + coerce fence
        let st = s.idx_stats(b"score_idx").unwrap();
        assert_eq!(st.entries, 30, "29 numeric + new");
        assert_eq!(st.coerce_failures, 1);
        assert_eq!(s.idx_list().len(), 1);
        // another row lands at the same value (duplicate handling)
        s.hset(b"row:extra", &[(b"score", b"50")]).unwrap();
        assert_eq!(
            s.idx_count(b"score_idx", &IndexValue::I64(50), &IndexValue::I64(50)).unwrap(),
            3,
            "row:5 + row:new + row:extra all at 50"
        );
    }
    // reopen: catalog persisted, segments rebuild lazily on first touch
    let s2 = Store::open(Config::default().with_persist(&dir).with_ttl_reaper_manual()).unwrap();
    let st = s2.idx_stats(b"score_idx").unwrap();
    assert_eq!(st.entries, 31, "30 + copied, rebuilt from replayed data");
    assert!(s2.idx_drop(b"score_idx"));
    assert!(s2.idx_query(b"score_idx", &IndexValue::I64(0), &IndexValue::I64(1), None, 10).is_err());
    drop(s2);
    let _ = std::fs::remove_dir_all(&dir);
}
