//! AOF-rewrite tests: full-keyspace reconstruction, atomic log swap,
//! size accounting, the non-blocking (concurrent) rewrite, and the
//! stream consumer-group section. Split from `tests_aof.rs` to keep
//! both under the 500-LOC house rule.

use super::*;
use crate::tests_aof::temp_aof;
use std::borrow::Cow;
use std::time::Duration;

// ───────────── AOF rewrite (Wave 2 #3) ─────────────

/// Tiny dispatch helper for AOF-rewrite roundtrip tests: turn the
/// canonical mutating verbs the rewriter emits back into Store mutations.
/// Mirrors a subset of kevy's dispatch — enough for the verbs
/// `dump_store_to_aof` actually emits.
pub(crate) fn apply_for_test(store: &mut Store, args: &Argv) {
    let verb = args[0].to_ascii_uppercase();
    match verb.as_slice() {
        b"SET" => {
            store.set(&args[1], args[2].to_vec(), None, false, false);
        }
        b"DEL" => {
            let keys: Vec<&[u8]> = args.iter().skip(1).collect();
            store.del(&keys);
        }
        b"HSET" => {
            let mut pairs: Vec<(&[u8], &[u8])> = Vec::new();
            let mut i = 2;
            while i + 1 < args.len() {
                pairs.push((&args[i], &args[i + 1]));
                i += 2;
            }
            store.hset(&args[1], &pairs).unwrap();
        }
        b"RPUSH" => {
            let items: Vec<&[u8]> = args.iter().skip(2).collect();
            store.rpush(&args[1], &items).unwrap();
        }
        b"SADD" => {
            let members: Vec<&[u8]> = args.iter().skip(2).collect();
            store.sadd(&args[1], &members).unwrap();
        }
        b"ZADD" => {
            let mut pairs: Vec<(f64, &[u8])> = Vec::new();
            let mut i = 2;
            while i + 1 < args.len() {
                let score: f64 = std::str::from_utf8(&args[i]).unwrap().parse().unwrap();
                pairs.push((score, &args[i + 1]));
                i += 2;
            }
            store.zadd(&args[1], &pairs).unwrap();
        }
        b"PEXPIRE" => {
            let ms: u64 = std::str::from_utf8(&args[2]).unwrap().parse().unwrap();
            store.expire(&args[1], Duration::from_millis(ms));
        }
        b"PEXPIREAT" => {
            // The rewrite emits absolute deadlines (never relative).
            let deadline: u64 = std::str::from_utf8(&args[2]).unwrap().parse().unwrap();
            store.expire_at_unix_ms(&args[1], deadline);
        }
        b"HPEXPIREAT" => {
            // Fixed rewrite shape: HPEXPIREAT key deadline FIELDS 1 field.
            assert_eq!(args[3].to_ascii_uppercase(), b"FIELDS");
            let deadline: u64 = std::str::from_utf8(&args[2]).unwrap().parse().unwrap();
            store.load_hash_field_ttl(&args[1], &args[5], deadline);
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
            store
                .xsetid(&args[1], last, Some(added), Some(mxd))
                .unwrap();
        }
        b"XGROUP" => match args[1].to_ascii_uppercase().as_slice() {
            b"CREATE" => {
                assert_eq!(args[5].to_ascii_uppercase(), b"MKSTREAM");
                let at = kevy_store::parse_explicit_id(&args[4], false).unwrap();
                store
                    .xgroup_create(
                        &args[2],
                        &args[3],
                        kevy_store::GroupCreateMode::AtId(at),
                        true,
                    )
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
                retrycount_override: Some(std::str::from_utf8(&args[9]).unwrap().parse().unwrap()),
                force: true,
                justid: true,
            };
            store
                .xclaim(&args[1], &args[2], &args[3], &[id], &opts, 0)
                .unwrap();
        }
        other => panic!(
            "unexpected verb in AOF rewrite: {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

#[test]
fn rewrite_reconstructs_full_keyspace() {
    let path = temp_aof("rewrite-all");

    let mut src = Store::new();
    src.set(b"str", b"hello".to_vec(), None, false, false);
    src.set(b"binary", vec![0u8, 1, 2, 255], None, false, false);
    src.hset(
        b"hash",
        &[
            (b"f1".as_slice(), b"v1".as_slice()),
            (b"f2".as_slice(), b"v2".as_slice()),
        ],
    )
    .unwrap();
    src.rpush(
        b"list",
        &[b"i1".as_slice(), b"i2".as_slice(), b"i3".as_slice()],
    )
    .unwrap();
    src.sadd(b"set", &[b"m1".as_slice(), b"m2".as_slice()])
        .unwrap();
    src.zadd(b"zset", &[(1.5, b"a".as_slice()), (2.5, b"b".as_slice())])
        .unwrap();
    src.set(
        b"ttl",
        b"x".to_vec(),
        Some(Duration::from_hours(1)),
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
    assert_eq!(dst.get(b"str").unwrap(), Some(Cow::Borrowed(&b"hello"[..])));
    assert_eq!(
        dst.get(b"binary").unwrap(),
        Some(Cow::Borrowed(&[0u8, 1, 2, 255][..]))
    );
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
    assert!(
        new_size < big_size,
        "rewrite should shrink: {new_size} vs {big_size}"
    );

    // Step 3: appending after rewrite lands in the new file.
    aof.append(&Argv::from(vec![
        b"SET".to_vec(),
        b"third".to_vec(),
        b"v".to_vec(),
    ]))
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
    // Fresh AOF carries the 9-byte AOF_MAGIC header.
    let base = aof.size_bytes();
    aof.append(&Argv::from(vec![
        b"SET".to_vec(),
        b"k".to_vec(),
        b"v".to_vec(),
    ]))
    .unwrap();
    let after_one = aof.size_bytes();
    assert!(after_one > base);
    aof.append(&Argv::from(vec![
        b"SET".to_vec(),
        b"k2".to_vec(),
        b"v".to_vec(),
    ]))
    .unwrap();
    assert!(aof.size_bytes() > after_one);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rewrite_resets_size_anchor() {
    let path = temp_aof("size-anchor");
    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    for _ in 0..10 {
        aof.append(&Argv::from(vec![
            b"SET".to_vec(),
            b"k".to_vec(),
            b"v".to_vec(),
        ]))
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
    assert_eq!(
        dst.get(b"b").unwrap(),
        Some(Cow::Borrowed(&b"22"[..])),
        "overwrite must win"
    );
    assert_eq!(
        dst.get(b"c").unwrap(),
        Some(Cow::Borrowed(&b"3"[..])),
        "new key must survive"
    );
    let _ = std::fs::remove_file(&path);
}

fn argv(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

// ───────────── stream consumer groups in the rewrite ─────────────

/// The four stream shapes the rewrite must reconstruct: a live group with
/// a tombstoned PEL row, a deleted-tail stream (scalars only), a
/// deleted-only stream carrying a group, and a virgin MKSTREAM-only key.
#[test]
fn rewrite_reconstructs_stream_groups() {
    use kevy_store::{GroupCreateMode, ReadGroupId, StreamId, XAddIdSpec};
    let id = |ms, seq| StreamId { ms, seq };
    let f = |k: &str| (k.as_bytes().to_vec(), vec![(b"f".to_vec(), b"v".to_vec())]);
    let path = temp_aof("rewrite-groups");

    let mut src = Store::new();
    // st: 3 entries, c1 holds 1-1+2-1 (t=1000), c2 holds 3-1 (t=2000),
    // then 2-1 deleted → tombstone PEL row.
    for ms in [1u64, 2, 3] {
        let (k, fields) = f("st");
        src.xadd(&k, XAddIdSpec::Explicit(id(ms, 1)), fields, false, 0)
            .unwrap();
    }
    src.xgroup_create(b"st", b"g", GroupCreateMode::AtId(StreamId::MIN), false)
        .unwrap();
    src.xreadgroup(b"st", b"g", b"c1", ReadGroupId::New, Some(2), false, 1000)
        .unwrap();
    src.xreadgroup(b"st", b"g", b"c2", ReadGroupId::New, None, false, 2000)
        .unwrap();
    src.xdel(b"st", &[id(2, 1)]).unwrap();
    // deltail: groupless, tail entry deleted → scalars need XSETID.
    for ms in [7u64, 8] {
        let (k, fields) = f("deltail");
        src.xadd(&k, XAddIdSpec::Explicit(id(ms, 1)), fields, false, 0)
            .unwrap();
    }
    src.xdel(b"deltail", &[id(8, 1)]).unwrap();
    // emptyg: every entry deleted, but a group remains.
    let (k, fields) = f("emptyg");
    src.xadd(&k, XAddIdSpec::Explicit(id(5, 1)), fields, false, 0)
        .unwrap();
    src.xdel(b"emptyg", &[id(5, 1)]).unwrap();
    src.xgroup_create(b"emptyg", b"g2", GroupCreateMode::AtId(id(5, 1)), false)
        .unwrap();
    // virgin: never had an entry, group created via MKSTREAM.
    src.xgroup_create(b"virgin", b"g3", GroupCreateMode::AtId(StreamId::MIN), true)
        .unwrap();

    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    aof.rewrite_from(&src).unwrap();
    drop(aof);

    let mut dst = Store::new();
    replay_aof(&path, |args| apply_for_test(&mut dst, &args)).unwrap();

    // st — full group fidelity minus the tombstone (XCLAIM cannot
    // recreate a PEL row for a deleted entry; documented trade-off).
    let v = dst.stream_view(b"st").unwrap().unwrap();
    assert_eq!(
        (
            v.length(),
            v.last_id(),
            v.entries_added(),
            v.max_deleted_id()
        ),
        (2, id(3, 1), 3, id(2, 1))
    );
    let g = v.group(b"g").expect("group must survive the rewrite");
    assert_eq!(g.last_delivered_id(), id(3, 1));
    assert_eq!(g.pending_count(), 2); // 2-1 tombstone dropped by design
    let p1 = g.pel.get(&id(1, 1)).unwrap();
    assert_eq!(
        (
            p1.consumer.as_slice(),
            p1.delivery_time_ms,
            p1.delivery_count
        ),
        (&b"c1"[..], 1000, 1)
    );
    let p3 = g.pel.get(&id(3, 1)).unwrap();
    assert_eq!(
        (
            p3.consumer.as_slice(),
            p3.delivery_time_ms,
            p3.delivery_count
        ),
        (&b"c2"[..], 2000, 1)
    );
    let mut consumers: Vec<(Vec<u8>, usize)> = g
        .consumers_iter()
        .map(|(n, c)| (n.to_vec(), c.pending_count()))
        .collect();
    consumers.sort();
    assert_eq!(consumers, vec![(b"c1".to_vec(), 1), (b"c2".to_vec(), 1)]);

    // deltail — deleted tail must not roll the ID clock back.
    let v = dst.stream_view(b"deltail").unwrap().unwrap();
    assert_eq!(
        (
            v.length(),
            v.last_id(),
            v.entries_added(),
            v.max_deleted_id()
        ),
        (1, id(8, 1), 2, id(8, 1))
    );

    // emptyg — empty stream with a group used to vanish entirely.
    let v = dst.stream_view(b"emptyg").unwrap().unwrap();
    assert_eq!(
        (
            v.length(),
            v.last_id(),
            v.entries_added(),
            v.max_deleted_id()
        ),
        (0, id(5, 1), 1, id(5, 1))
    );
    assert_eq!(v.group(b"g2").unwrap().last_delivered_id(), id(5, 1));

    // virgin — groups-only stream survives via XGROUP … MKSTREAM.
    let v = dst.stream_view(b"virgin").unwrap().unwrap();
    assert_eq!((v.length(), v.last_id()), (0, StreamId::MIN));
    assert!(v.group(b"g3").is_some());

    let _ = std::fs::remove_file(&path);
}

/// The tailgate storm's crash class: a big collection must rewrite as
/// MANY bounded frames (Redis's 64-items-per-command batching), never
/// one giant Argv whose u32 offset table can wrap. 200 list items →
/// ceil(200/64) = 4 RPUSH frames, order preserved end to end; 100 hash
/// pairs → 2 HSET frames with no pair split across a boundary.
#[test]
fn rewrite_chunks_large_collections() {
    use kevy_store::Store;
    let mut store = Store::new();
    let items: Vec<Vec<u8>> = (0..200u32)
        .map(|i| format!("item-{i:03}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = items.iter().map(Vec::as_slice).collect();
    store.rpush(b"biglist", &refs).unwrap();
    let fields: Vec<(Vec<u8>, Vec<u8>)> = (0..100u32)
        .map(|i| {
            (
                format!("f{i:03}").into_bytes(),
                format!("v{i}").into_bytes(),
            )
        })
        .collect();
    let pairs: Vec<(&[u8], &[u8])> = fields
        .iter()
        .map(|(f, v)| (f.as_slice(), v.as_slice()))
        .collect();
    store.hset(b"bighash", &pairs).unwrap();

    let (buf, keys) = crate::dump_store_to_buf(&store, crate::AofFormat::V1);
    assert_eq!(keys, 2);
    let text = String::from_utf8_lossy(&buf);
    assert_eq!(text.matches("RPUSH").count(), 4, "200 items / 64 per frame");
    assert_eq!(text.matches("HSET").count(), 2, "100 pairs / 64 per frame");

    // Replay reconstructs the exact values — order and pairing survive
    // the chunk boundaries.
    let mut back = Store::new();
    let mut pos = crate::aof::AOF_MAGIC.len();
    let mut argv = kevy_resp::Argv::default();
    while pos < buf.len() {
        let Ok(Some(used)) = kevy_resp::parse_command_into(&buf[pos..], &mut argv) else {
            break;
        };
        let args: Vec<Vec<u8>> = argv.iter().map(<[u8]>::to_vec).collect();
        match args[0].as_slice() {
            b"RPUSH" => {
                let items: Vec<&[u8]> = args[2..].iter().map(Vec::as_slice).collect();
                back.rpush(&args[1], &items).unwrap();
            }
            b"HSET" => {
                let pairs: Vec<(&[u8], &[u8])> = args[2..]
                    .chunks(2)
                    .map(|fv| (fv[0].as_slice(), fv[1].as_slice()))
                    .collect();
                back.hset(&args[1], &pairs).unwrap();
            }
            other => panic!("unexpected verb {:?}", String::from_utf8_lossy(other)),
        }
        pos += used;
    }
    assert_eq!(
        back.lrange(b"biglist", 0, -1).unwrap(),
        store.lrange(b"biglist", 0, -1).unwrap()
    );
    // hgetall answers in table order, which differs by insertion
    // history — compare as sets of pairs.
    let pairs_of = |flat: Vec<Vec<u8>>| {
        let mut ps: Vec<(Vec<u8>, Vec<u8>)> = flat
            .chunks(2)
            .map(|fv| (fv[0].clone(), fv[1].clone()))
            .collect();
        ps.sort();
        ps
    };
    assert_eq!(
        pairs_of(back.hgetall(b"bighash").unwrap()),
        pairs_of(store.hgetall(b"bighash").unwrap())
    );
}

/// The baseline estimator serialises through the same emitters a real
/// rewrite uses, so on an untiered store its count equals the in-memory
/// image's exact size — the anchor the short-lived-process fix relies on.
#[test]
fn estimate_matches_the_real_dump() {
    let mut store = Store::new();
    apply_for_test(&mut store, &argv(&[b"SET", b"k1", b"value-one"]));
    apply_for_test(
        &mut store,
        &argv(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"]),
    );
    apply_for_test(&mut store, &argv(&[b"RPUSH", b"l", b"a", b"b", b"c"]));
    apply_for_test(&mut store, &argv(&[b"SADD", b"s", b"m1", b"m2"]));
    apply_for_test(&mut store, &argv(&[b"ZADD", b"z", b"1.5", b"member"]));
    let (buf, _) = crate::dump_store_to_buf(&store, crate::AofFormat::V2);
    assert_eq!(
        crate::estimate_rewrite_size(&store),
        buf.len() as u64,
        "counting writer must agree with the real serialiser byte-for-byte"
    );
}

/// `replay_aof_quiet` returns the same report as the loud path — only
/// the stderr line differs.
#[test]
fn quiet_replay_reports_identically() {
    let path = temp_aof("quiet-replay");
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&argv(&[b"SET", b"a", b"1"])).unwrap();
        aof.append(&argv(&[b"SET", b"b", b"2"])).unwrap();
    }
    let mut loud = 0u64;
    let r1 = crate::replay_aof(&path, |_| loud += 1).unwrap();
    let mut quiet = 0u64;
    let r2 = crate::replay_aof_quiet(&path, false, |_| quiet += 1).unwrap();
    assert_eq!(loud, quiet);
    assert_eq!(r1.commands, r2.commands);
    assert_eq!(r1.bytes, r2.bytes);
    assert_eq!(r1.replayed_bytes, r2.replayed_bytes);
    std::fs::remove_file(&path).ok();
}

/// The two-phase handoff (S4): early tee generations are appended to the
/// tmp image off-thread (simulated here), later writes keep teeing into a
/// fresh generation, and the final swap applies only the last one. Replay
/// must see every write exactly once, in order.
#[test]
fn two_phase_handoff_replays_every_generation_once() {
    let path = temp_aof("handoff-rw");
    let mut store = Store::new();
    store.set(b"a", b"1".to_vec(), None, false, false);

    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    let plan = aof.begin_concurrent_rewrite(&store).unwrap();

    // Generation 1: writes during the off-lock spill.
    aof.append(&argv(&[b"SET", b"g1", b"x"])).unwrap();
    aof.append(&argv(&[b"SET", b"k", b"gen1"])).unwrap();
    std::fs::write(&plan.tmp, &plan.body).unwrap();

    // Handoff: the driver takes gen 1 for the worker; the tee restarts.
    let gen1 = aof.take_tee_for_handoff().unwrap();
    assert!(!gen1.is_empty());
    assert!(aof.is_rewriting(), "handoff must keep the rewrite live");

    // Generation 2: writes during the worker's off-thread append.
    aof.append(&argv(&[b"SET", b"g2", b"y"])).unwrap();
    aof.append(&argv(&[b"SET", b"k", b"gen2"])).unwrap();

    // Worker lands gen 1 (append + fsync), exactly like PersistJob::TeeAppend.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&plan.tmp)
            .unwrap();
        f.write_all(&gen1).unwrap();
        f.sync_all().unwrap();
    }

    // Final swap with the (small) last generation.
    let gen2 = aof.take_tee_for_handoff().unwrap();
    let stats = aof
        .finish_concurrent_rewrite_with(&plan.tmp, plan.keys, gen2)
        .unwrap();
    assert!(!aof.is_rewriting());
    assert_eq!(stats.keys, 1);

    let mut dst = Store::new();
    replay_aof(&path, |a| apply_for_test(&mut dst, &a)).unwrap();
    assert_eq!(dst.get(b"a").unwrap(), Some(Cow::Borrowed(&b"1"[..])));
    assert_eq!(
        dst.get(b"g1").unwrap(),
        Some(Cow::Borrowed(&b"x"[..])),
        "gen-1 write must survive"
    );
    assert_eq!(
        dst.get(b"g2").unwrap(),
        Some(Cow::Borrowed(&b"y"[..])),
        "gen-2 write must survive"
    );
    assert_eq!(
        dst.get(b"k").unwrap(),
        Some(Cow::Borrowed(&b"gen2"[..])),
        "generations must replay in order (gen 2 wins)"
    );
    let _ = std::fs::remove_file(&path);
}

/// A divergence-deferred rewrite re-anchors the auto-rewrite growth
/// rule at the CURRENT size — retrying immediately would diverge the
/// same way, so the next attempt must wait for another growth factor.
#[test]
fn deferred_rewrite_reanchors_at_current_size() {
    let path = temp_aof("defer-anchor");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    for i in 0..100 {
        let key = format!("k{i}");
        aof.append(&argv(&[b"SET", key.as_bytes(), b"0123456789abcdef"])).unwrap();
    }
    assert!(
        aof.size_bytes() > aof.size_at_last_rewrite(),
        "log must have outgrown the anchor"
    );
    aof.anchor_rewrite_deferred();
    assert_eq!(
        aof.size_bytes(),
        aof.size_at_last_rewrite(),
        "deferral must anchor the growth rule at the current size"
    );
    let _ = std::fs::remove_file(&path);
}

/// The tee pool (S5-F): a returned generation buffer is reused by the
/// next `take_tee_for_handoff` (warm pages, no fresh mapping), and
/// teardown drains every retained buffer for the off-thread drop.
#[test]
fn tee_pool_recycles_buffers_and_teardown_drains() {
    let path = temp_aof("tee-pool");
    let mut store = Store::new();
    store.set(b"a", b"1".to_vec(), None, false, false);
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    let plan = aof.begin_concurrent_rewrite(&store).unwrap();

    aof.append(&argv(&[b"SET", b"g1", b"x"])).unwrap();
    let gen1 = aof.take_tee_for_handoff().unwrap();
    assert!(!gen1.is_empty());

    // Worker returns the buffer cleared; remember its identity.
    let mut returned = gen1;
    returned.clear();
    let cap_marker = {
        returned.reserve(1 << 20); // give it a recognizable capacity
        returned.capacity()
    };
    aof.stash_tee_spare(returned);

    // The stash installs at the NEXT take (ping-pong is one step
    // deep): gen2 still grows in the take-1-installed buffer; the
    // recycled 1 MiB one becomes the live tee at take 2 and comes back
    // as generation 3.
    aof.append(&argv(&[b"SET", b"g2", b"y"])).unwrap();
    let gen2 = aof.take_tee_for_handoff().unwrap();
    aof.append(&argv(&[b"SET", b"g3", b"z"])).unwrap();
    let gen3 = aof.take_tee_for_handoff().unwrap();
    assert_eq!(gen3.capacity(), cap_marker, "handoff must reuse the pooled buffer");

    aof.stash_tee_spare(gen2);
    aof.stash_tee_spare(gen3); // bigger buffer wins the slot, gen2 drops
    let bufs = aof.take_tee_teardown();
    assert!(
        bufs.iter().any(|b| b.capacity() == cap_marker),
        "teardown must drain the retained warm buffer"
    );
    assert!(aof.take_tee_teardown().is_empty(), "teardown drains everything once");
    aof.abort_concurrent_rewrite();
    assert!(!aof.is_rewriting());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&plan.tmp);
}

/// File-backed tee (S5-G): records stage → ring-style positioned
/// writes into `<aof>.tee` → worker-style fold into the image →
/// bounded final swap. Replay must see the image plus every diff byte
/// exactly once, in order.
#[cfg(unix)]
#[test]
fn file_tee_roundtrip_replays_every_generation() {
    use std::os::unix::fs::FileExt;
    let path = temp_aof("filetee-rw");
    let mut store = Store::new();
    store.set(b"a", b"1".to_vec(), None, false, false);

    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    // The image (worker-side in production): dump the store to tmp.
    let tmp = aof.begin_view_rewrite_filetee().unwrap();
    assert!(aof.is_rewriting());
    crate::dump_aof(&tmp, &store).unwrap();

    // Diff generation 1, driver-style: stage → chunk → positioned write.
    aof.append(&argv(&[b"SET", b"g1", b"x"])).unwrap();
    aof.append(&argv(&[b"SET", b"k", b"gen1"])).unwrap();
    let (off, chunk, _fd) = aof.take_tee_pending().unwrap();
    assert_eq!(off, 0);
    let tee_handle = aof.tee_copy_handle().unwrap().unwrap();
    tee_handle.write_all_at(&chunk, off).unwrap();
    aof.stash_tee_spare(chunk);

    // Worker fold of [consumed, handed).
    let (consumed, handed) = aof.tee_watermarks().unwrap();
    assert_eq!((consumed, handed), (0, off + tee_handle.metadata().unwrap().len()));
    {
        use std::io::{Read, Seek, Write};
        let mut src = tee_handle.try_clone().unwrap();
        src.seek(std::io::SeekFrom::Start(consumed)).unwrap();
        let mut dst = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
        let mut take = src.take(handed - consumed);
        std::io::copy(&mut take, &mut dst).unwrap();
        dst.flush().unwrap();
    }
    aof.tee_advance_consumed(handed);

    // Diff generation 2 stays in staging — the final swap flushes it.
    aof.append(&argv(&[b"SET", b"g2", b"y"])).unwrap();
    aof.append(&argv(&[b"SET", b"k", b"gen2"])).unwrap();
    let (stats, tee_path) = aof.finish_concurrent_rewrite_from_tee(&tmp, 1).unwrap();
    assert!(!aof.is_rewriting());
    assert_eq!(stats.keys, 1);
    let _ = std::fs::remove_file(&tee_path);

    let mut dst = Store::new();
    replay_aof(&path, |a| apply_for_test(&mut dst, &a)).unwrap();
    assert_eq!(dst.get(b"a").unwrap(), Some(Cow::Borrowed(&b"1"[..])));
    assert_eq!(dst.get(b"g1").unwrap(), Some(Cow::Borrowed(&b"x"[..])), "folded gen-1 must survive");
    assert_eq!(dst.get(b"g2").unwrap(), Some(Cow::Borrowed(&b"y"[..])), "staged gen-2 must survive");
    assert_eq!(
        dst.get(b"k").unwrap(),
        Some(Cow::Borrowed(&b"gen2"[..])),
        "generations must replay in order"
    );
}
