//! AOF-rewrite tests: full-keyspace reconstruction, atomic log swap,
//! size accounting, the non-blocking (concurrent) rewrite, and the
//! stream consumer-group section. Split from `tests_aof.rs` to keep
//! both under the 500-LOC house rule.

use super::*;
use crate::tests_aof::temp_aof;
use std::time::Duration;

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
        b"DEL" => {
            let keys: Vec<Vec<u8>> = args.iter().skip(1).map(|a| a.to_vec()).collect();
            store.del(&keys);
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
        b"XADD" => {
            // Two rewrite shapes: `XADD key id f v …` and the empty-stream
            // re-creation trick `XADD key MAXLEN 0 id x x`.
            let mut i = 2;
            let mut maxlen: Option<u64> = None;
            if args[i].eq_ignore_ascii_case(b"MAXLEN") {
                maxlen = Some(std::str::from_utf8(&args[3]).unwrap().parse().unwrap());
                i = 4;
            }
            let spec = kevy_store::parse_xadd_id(&args[i]).unwrap();
            let mut fields = Vec::new();
            let mut j = i + 1;
            while j + 1 < args.len() {
                fields.push((args[j].to_vec(), args[j + 1].to_vec()));
                j += 2;
            }
            store.xadd(&args[1], spec, fields, false, 0).unwrap();
            if let Some(n) = maxlen {
                store.xtrim_maxlen(&args[1], n).unwrap();
            }
        }
        b"XSETID" => {
            let last = kevy_store::parse_explicit_id(&args[2], false).unwrap();
            assert_eq!(args[3].to_ascii_uppercase(), b"ENTRIESADDED");
            let added: u64 = std::str::from_utf8(&args[4]).unwrap().parse().unwrap();
            assert_eq!(args[5].to_ascii_uppercase(), b"MAXDELETEDID");
            let mxd = kevy_store::parse_explicit_id(&args[6], false).unwrap();
            store.xsetid(&args[1], last, Some(added), Some(mxd)).unwrap();
        }
        b"XGROUP" => match args[1].to_ascii_uppercase().as_slice() {
            b"CREATE" => {
                assert_eq!(args[5].to_ascii_uppercase(), b"MKSTREAM");
                let at = kevy_store::parse_explicit_id(&args[4], false).unwrap();
                store
                    .xgroup_create(&args[2], &args[3], kevy_store::GroupCreateMode::AtId(at), true)
                    .unwrap();
            }
            b"CREATECONSUMER" => {
                store
                    .xgroup_create_consumer(&args[2], &args[3], &args[4], 7_777)
                    .unwrap();
            }
            other => panic!(
                "unexpected XGROUP sub in AOF rewrite: {:?}",
                String::from_utf8_lossy(other)
            ),
        },
        b"XCLAIM" => {
            // Fixed rewrite shape:
            // XCLAIM key g consumer 0 id TIME t RETRYCOUNT n FORCE JUSTID
            assert_eq!(&args[4], b"0");
            assert_eq!(args[6].to_ascii_uppercase(), b"TIME");
            assert_eq!(args[8].to_ascii_uppercase(), b"RETRYCOUNT");
            assert_eq!(args[10].to_ascii_uppercase(), b"FORCE");
            assert_eq!(args[11].to_ascii_uppercase(), b"JUSTID");
            let id = kevy_store::parse_explicit_id(&args[5], false).unwrap();
            let opts = kevy_store::XClaimOpts {
                min_idle_ms: 0,
                idle_override_ms: None,
                time_override_ms: Some(std::str::from_utf8(&args[7]).unwrap().parse().unwrap()),
                retrycount_override: Some(
                    std::str::from_utf8(&args[9]).unwrap().parse().unwrap(),
                ),
                force: true,
                justid: true,
            };
            store.xclaim(&args[1], &args[2], &args[3], &[id], &opts, 0).unwrap();
        }
        other => panic!("unexpected verb in AOF rewrite: {:?}", String::from_utf8_lossy(other)),
    }
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

/// The non-blocking rewrite must lose nothing: writes that land *between*
/// `begin_concurrent_rewrite` (snapshot taken) and `finish_concurrent_rewrite`
/// (swap) — i.e. during the off-lock disk spill — are tee'd into the diff
/// buffer and replayed after the compacted snapshot.
#[test]
fn concurrent_rewrite_captures_writes_during_spill() {
    let path = temp_aof("concurrent-rw");
    let mut store = Store::new();
    store.set(b"a", b"1".to_vec(), None, false, false);
    store.set(b"b", b"2".to_vec(), None, false, false);

    let mut aof = Aof::open(&path, Fsync::Always).unwrap();

    // Phase 1 (would be under the store lock): snapshot {a,b}, start teeing.
    let plan = aof.begin_concurrent_rewrite(&store).unwrap();
    assert!(aof.is_rewriting());
    assert_eq!(plan.keys, 2);

    // Writes that arrive DURING the off-lock spill — must be captured by the
    // tee, not lost when the snapshot (which predates them) is swapped in.
    aof.append(&argv(&[b"SET", b"c", b"3"])).unwrap(); // new key
    aof.append(&argv(&[b"SET", b"b", b"22"])).unwrap(); // overwrite
    aof.append(&argv(&[b"DEL", b"a"])).unwrap(); // delete a snapshotted key

    // Phase 2: spill the snapshot image to the temp file (off-lock).
    std::fs::write(&plan.tmp, &plan.body).unwrap();

    // Phase 3: append the diff + atomic swap.
    let stats = aof.finish_concurrent_rewrite(&plan.tmp, plan.keys).unwrap();
    assert!(!aof.is_rewriting());
    assert_eq!(stats.keys, 2);
    assert_eq!(aof.rewrites_total(), 1);

    // Replay the rewritten AOF: compacted snapshot THEN the during-spill diff.
    let mut dst = Store::new();
    replay_aof(&path, |a| apply_for_test(&mut dst, &a)).unwrap();
    assert_eq!(dst.get(b"a").unwrap(), None, "DEL during spill must apply");
    assert_eq!(dst.get(b"b").unwrap(), Some(&b"22"[..]), "overwrite must win");
    assert_eq!(dst.get(b"c").unwrap(), Some(&b"3"[..]), "new key must survive");
    let _ = std::fs::remove_file(&path);
}

fn argv(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

// ───────────── stream consumer groups in the rewrite (2026-06-11) ─────────────

/// The four stream shapes the rewrite must reconstruct: a live group with
/// a tombstoned PEL row, a deleted-tail stream (scalars only), a
/// deleted-only stream carrying a group, and a virgin MKSTREAM-only key.
#[test]
fn rewrite_reconstructs_stream_groups() {
    use kevy_store::{GroupCreateMode, ReadGroupId, StreamId, XAddIdSpec};
    let id = |ms, seq| StreamId { ms, seq };
    let f = |k: &str| {
        (
            k.as_bytes().to_vec(),
            vec![(b"f".to_vec(), b"v".to_vec())],
        )
    };
    let path = temp_aof("rewrite-groups");

    let mut src = Store::new();
    // st: 3 entries, c1 holds 1-1+2-1 (t=1000), c2 holds 3-1 (t=2000),
    // then 2-1 deleted → tombstone PEL row.
    for ms in [1u64, 2, 3] {
        let (k, fields) = f("st");
        src.xadd(&k, XAddIdSpec::Explicit(id(ms, 1)), fields, false, 0).unwrap();
    }
    src.xgroup_create(b"st", b"g", GroupCreateMode::AtId(StreamId::MIN), false).unwrap();
    src.xreadgroup(b"st", b"g", b"c1", ReadGroupId::New, Some(2), false, 1000).unwrap();
    src.xreadgroup(b"st", b"g", b"c2", ReadGroupId::New, None, false, 2000).unwrap();
    src.xdel(b"st", &[id(2, 1)]).unwrap();
    // deltail: groupless, tail entry deleted → scalars need XSETID.
    for ms in [7u64, 8] {
        let (k, fields) = f("deltail");
        src.xadd(&k, XAddIdSpec::Explicit(id(ms, 1)), fields, false, 0).unwrap();
    }
    src.xdel(b"deltail", &[id(8, 1)]).unwrap();
    // emptyg: every entry deleted, but a group remains.
    let (k, fields) = f("emptyg");
    src.xadd(&k, XAddIdSpec::Explicit(id(5, 1)), fields, false, 0).unwrap();
    src.xdel(b"emptyg", &[id(5, 1)]).unwrap();
    src.xgroup_create(b"emptyg", b"g2", GroupCreateMode::AtId(id(5, 1)), false).unwrap();
    // virgin: never had an entry, group created via MKSTREAM.
    src.xgroup_create(b"virgin", b"g3", GroupCreateMode::AtId(StreamId::MIN), true).unwrap();

    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    aof.rewrite_from(&src).unwrap();
    drop(aof);

    let mut dst = Store::new();
    replay_aof(&path, |args| apply_for_test(&mut dst, &args)).unwrap();

    // st — full group fidelity minus the tombstone (RFC 2026-06-11).
    let v = dst.stream_view(b"st").unwrap().unwrap();
    assert_eq!(
        (v.length(), v.last_id(), v.entries_added(), v.max_deleted_id()),
        (2, id(3, 1), 3, id(2, 1))
    );
    let g = v.group(b"g").expect("group must survive the rewrite");
    assert_eq!(g.last_delivered_id(), id(3, 1));
    assert_eq!(g.pending_count(), 2); // 2-1 tombstone dropped by design
    let p1 = g.pel.get(&id(1, 1)).unwrap();
    assert_eq!(
        (p1.consumer.as_slice(), p1.delivery_time_ms, p1.delivery_count),
        (&b"c1"[..], 1000, 1)
    );
    let p3 = g.pel.get(&id(3, 1)).unwrap();
    assert_eq!(
        (p3.consumer.as_slice(), p3.delivery_time_ms, p3.delivery_count),
        (&b"c2"[..], 2000, 1)
    );
    let mut consumers: Vec<(Vec<u8>, usize)> =
        g.consumers_iter().map(|(n, c)| (n.to_vec(), c.pending_count())).collect();
    consumers.sort();
    assert_eq!(consumers, vec![(b"c1".to_vec(), 1), (b"c2".to_vec(), 1)]);

    // deltail — deleted tail must not roll the ID clock back.
    let v = dst.stream_view(b"deltail").unwrap().unwrap();
    assert_eq!(
        (v.length(), v.last_id(), v.entries_added(), v.max_deleted_id()),
        (1, id(8, 1), 2, id(8, 1))
    );

    // emptyg — empty stream with a group used to vanish entirely.
    let v = dst.stream_view(b"emptyg").unwrap().unwrap();
    assert_eq!(
        (v.length(), v.last_id(), v.entries_added(), v.max_deleted_id()),
        (0, id(5, 1), 1, id(5, 1))
    );
    assert_eq!(v.group(b"g2").unwrap().last_delivered_id(), id(5, 1));

    // virgin — groups-only stream survives via XGROUP … MKSTREAM.
    let v = dst.stream_view(b"virgin").unwrap().unwrap();
    assert_eq!((v.length(), v.last_id()), (0, StreamId::MIN));
    assert!(v.group(b"g3").is_some());

    let _ = std::fs::remove_file(&path);
}
