//! Tiering unit suite (capacity arc T3): codec round trips, the
//! demote/promote in-place contract (value + TTL + lru_clock + hfttl +
//! WATCH + events), WRONGTYPE-with-zero-preads, exact accounting, the
//! sampler's cold/non-spillable skips, the spill-batch bound (B3),
//! rename-then-compact survival, and the FLUSHALL wipe.

use crate::value::{COLD_TAG_HASH, COLD_TAG_STRING, Value};
use crate::{Store, StoreError, tier_codec};
use core::time::Duration;

/// Incompressible filler: several tier tests reason about ON-DISK
/// sizes and sealing, and the vlog now stores kevy-compress frames —
/// a constant run would collapse and break their size premises.
fn noise(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x9E37_79B9;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn tiered(name: &str, budget: u64) -> (Store, kevy_tmpdir::TmpDir) {
    let d = kevy_tmpdir::TmpDir::new(name);
    let mut s = Store::new();
    s.enable_tiering(d.path(), budget).unwrap();
    (s, d)
}

fn is_cold(s: &Store, key: &[u8]) -> bool {
    matches!(s.map.get(key).map(|e| &e.value), Some(Value::Cold(_)))
}

// ---- codec ----------------------------------------------------------

#[test]
fn codec_bulk_round_trip_incl_empty() {
    for payload in [vec![b'x'; 5000], Vec::new(), b"77".to_vec()] {
        let v = Value::ArcBulk(std::sync::Arc::new(payload.clone().into_boxed_slice()));
        let (enc, tag) = tier_codec::encode(&v).expect("bulk is spillable");
        assert_eq!(tag, COLD_TAG_STRING);
        let back = tier_codec::decode(tag, enc).unwrap();
        let bytes: Vec<u8> = match &back {
            Value::ArcBulk(a) => a.as_ref().to_vec(),
            Value::Str(s) => s.as_slice().to_vec(),
            Value::Int(n) => n.to_string().into_bytes(),
            other => panic!("unexpected decode variant {:?}", other.type_name()),
        };
        assert_eq!(bytes, payload);
    }
}

#[test]
fn codec_hash_round_trip_heap_inline_and_empty() {
    // Heap hash with a binary value + an empty-value field.
    let mut s = Store::new();
    s.hset(
        b"h",
        &[
            (b"name".as_slice(), b"ada".as_slice()),
            (b"blob".as_slice(), &[0u8, 255, 1, 2][..]),
            (b"empty".as_slice(), b"".as_slice()),
            (b"long-field-name-over-inline-budget".as_slice(), b"v".as_slice()),
        ],
    )
    .unwrap();
    let v = s.map.get(b"h".as_slice()).map(|e| e.value.clone()).unwrap();
    assert!(matches!(v, Value::Hash(_)), "4 pairs must be heap-backed");
    let (enc, tag) = tier_codec::encode(&v).unwrap();
    assert_eq!(tag, COLD_TAG_HASH);
    let Value::Hash(h) = tier_codec::decode(tag, enc).unwrap() else {
        panic!("hash decodes to heap hash")
    };
    assert_eq!(h.len(), 4);
    assert_eq!(h.get(b"name".as_slice()).unwrap().as_slice(), b"ada");
    assert_eq!(h.get(b"blob".as_slice()).unwrap().as_slice(), &[0u8, 255, 1, 2]);
    assert_eq!(h.get(b"empty".as_slice()).unwrap().as_slice(), b"");

    // Inline (SmallHashInline) encodes through the same wire shape.
    let mut s2 = Store::new();
    s2.hset(b"i", &[(b"a".as_slice(), b"1".as_slice())]).unwrap();
    let vi = s2.map.get(b"i".as_slice()).map(|e| e.value.clone()).unwrap();
    assert!(matches!(vi, Value::SmallHashInline(_)));
    let (enc, tag) = tier_codec::encode(&vi).unwrap();
    let Value::Hash(h) = tier_codec::decode(tag, enc).unwrap() else {
        panic!("inline hash decodes to heap hash")
    };
    assert_eq!(h.get(b"a".as_slice()).unwrap().as_slice(), b"1");

    // Empty hash payload (n = 0) — legal, round-trips.
    let (enc, tag) = tier_codec::encode(&Value::Hash(std::sync::Arc::default())).unwrap();
    let Value::Hash(h) = tier_codec::decode(tag, enc).unwrap() else { panic!() };
    assert_eq!(h.len(), 0);
}

// ---- demote/promote round trip --------------------------------------

#[test]
fn demote_promote_preserves_value_ttl_lru_watch_and_fires_no_events() {
    let (mut s, _d) = tiered("tier-roundtrip", u64::MAX);
    s.set_notify_capture(true, true, true);
    let big = vec![b'z'; 4096];
    s.set(b"k", big.clone(), Some(Duration::from_secs(600)), false, false);
    let ttl_before = s.pttl(b"k");
    let lru_before = s.map.get(b"k".as_slice()).unwrap().lru_clock();
    let watch_v = s.record_watch(b"k");
    drop(s.take_notify_events()); // clear the `new` capture from SET

    assert!(s.debug_force_demote(b"k"));
    assert!(is_cold(&s, b"k"));
    assert_eq!(s.map.get(b"k".as_slice()).unwrap().lru_clock(), lru_before, "demote must preserve lru_clock");
    assert_eq!(s.key_version(b"k"), watch_v, "demote must not bump WATCH");
    assert!(!s.has_notify_events(), "demote emits zero events");
    let ttl_cold = s.pttl(b"k");
    assert!(ttl_cold > 0 && ttl_cold <= ttl_before, "TTL rides the Entry, not the value");

    assert!(s.promote_in_place(b"k"));
    assert!(!is_cold(&s, b"k"));
    assert_eq!(s.key_version(b"k"), watch_v, "promote must not bump WATCH");
    assert!(!s.has_notify_events(), "promote emits zero events");
    assert_eq!(s.get(b"k").unwrap().unwrap().as_ref(), big.as_slice());
    assert!(s.pttl(b"k") > 0);
    let st = s.tier_stats();
    assert_eq!((st.demotions_total, st.promotions_total, st.cold_keys), (1, 1, 0));
    assert_eq!(s.evictions_total(), 0, "demotion is not eviction");
}

#[test]
fn hash_field_ttls_stay_in_ram_and_purge_on_promote() {
    let (mut s, _d) = tiered("tier-hfttl", u64::MAX);
    s.hset(
        b"h",
        &[
            (b"keep".as_slice(), b"1".as_slice()),
            (b"drop".as_slice(), b"2".as_slice()),
            (b"pad-the-hash-to-heap".as_slice(), b"3".as_slice()),
        ],
    )
    .unwrap();
    let now = crate::now_unix_ms();
    // `drop` expires while cold; `keep` far in the future.
    s.hexpire_at(b"h", &[b"drop"], now + 30, crate::HExpireCond::Always).unwrap();
    s.hexpire_at(b"h", &[b"keep"], now + 100_000, crate::HExpireCond::Always).unwrap();
    assert!(s.debug_force_demote(b"h"));
    std::thread::sleep(Duration::from_millis(50));
    // First hash access purges the expired-while-cold field (promotes
    // via the purge's HDEL) and must not resurrect it.
    assert!(!s.hexists(b"h", b"drop").unwrap());
    assert!(s.hexists(b"h", b"keep").unwrap());
    let ttls = s.hpttl(b"h", &[b"keep"]).unwrap();
    assert!(ttls[0] > 0, "surviving field TTL intact: {ttls:?}");
}

// ---- WRONGTYPE with zero preads -------------------------------------

#[test]
fn wrongtype_on_cold_never_reads_the_vlog() {
    let (mut s, _d) = tiered("tier-wrongtype", u64::MAX);
    s.set(b"str", vec![b'a'; 1024], None, false, false);
    s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice()), (b"pad-to-heap-hash".as_slice(), b"v".as_slice()), (b"third".as_slice(), b"v".as_slice())]).unwrap();
    assert!(s.debug_force_demote(b"str"));
    assert!(s.debug_force_demote(b"h"));
    let preads0 = s.tier_stats().preads_total;

    // Writers of every other type against a cold string: stage-1 tag
    // check refuses before any IO.
    assert_eq!(s.lpush(b"str", &[b"x".as_slice()]), Err(StoreError::WrongType));
    assert_eq!(s.sadd(b"str", &[b"x".as_slice()]), Err(StoreError::WrongType));
    assert_eq!(s.hset(b"str", &[(b"f".as_slice(), b"v".as_slice())]), Err(StoreError::WrongType));
    assert_eq!(s.incr_by(b"h", 1), Err(StoreError::WrongType));
    // Readers cross-typed: GET on a cold hash / HGET on a cold string.
    assert_eq!(s.get(b"h").err(), Some(StoreError::WrongType));
    assert_eq!(s.hget(b"str", b"f").err(), Some(StoreError::WrongType));

    assert_eq!(s.tier_stats().preads_total, preads0, "WRONGTYPE must not pread");
    assert!(is_cold(&s, b"str") && is_cold(&s, b"h"), "refusals must not materialize");
    assert_eq!(s.tier_stats().promotions_total, 0);
}

// ---- promotion gate --------------------------------------------------

#[test]
fn first_read_serves_without_installing_second_read_promotes() {
    let (mut s, _d) = tiered("tier-gate", u64::MAX);
    let big = vec![b'q'; 2048];
    s.set(b"k", big.clone(), None, false, false);
    assert!(s.debug_force_demote(b"k"));

    // 1st materializing access: identical bytes, still cold.
    assert_eq!(s.get(b"k").unwrap().unwrap().as_ref(), big.as_slice());
    assert!(is_cold(&s, b"k"), "first touch serves without installing");
    assert_eq!(s.tier_stats().promotions_total, 0);

    // 2nd: promotes.
    assert_eq!(s.get(b"k").unwrap().unwrap().as_ref(), big.as_slice());
    assert!(!is_cold(&s, b"k"), "second touch promotes");
    assert_eq!(s.tier_stats().promotions_total, 1);
}

#[test]
fn shared_lane_reads_never_promote_and_never_mark() {
    let (mut s, _d) = tiered("tier-shared", u64::MAX);
    let big = vec![b'w'; 2048];
    s.set(b"k", big.clone(), None, false, false);
    assert!(s.debug_force_demote(b"k"));
    for _ in 0..3 {
        let got = s.get_shared(b"k").unwrap().unwrap();
        assert_eq!(got.as_ref(), big.as_slice());
    }
    assert!(is_cold(&s, b"k"));
    assert_eq!(s.tier_stats().promotions_total, 0);
    // The probation mark stays clear: the NEXT &mut read is a first
    // touch (serve), not a promote.
    assert_eq!(s.get(b"k").unwrap().unwrap().as_ref(), big.as_slice());
    assert!(is_cold(&s, b"k"), "shared reads must not have set the mark");
}

// ---- accounting ------------------------------------------------------

#[test]
fn demote_and_promote_accounting_is_exact() {
    let (mut s, _d) = tiered("tier-account", u64::MAX);
    s.set(b"k", vec![b'a'; 8192], None, false, false);
    let used_hot = s.used_memory();
    let w_hot = s.map.get(b"k".as_slice()).unwrap().weight();
    assert!(s.debug_force_demote(b"k"));
    let w_cold = s.map.get(b"k".as_slice()).unwrap().weight();
    assert_eq!(w_cold, 0, "short key + stub owns zero heap");
    assert_eq!(s.used_memory(), used_hot - w_hot, "demote reclaims exactly the value weight");
    assert_eq!(s.estimate_key_bytes(b"k"), Some(crate::value::ENTRY_OVERHEAD), "MEMORY USAGE is stub-actual");
    assert_eq!(s.tier_stats().cold_bytes, w_hot);

    s.promote_in_place(b"k");
    assert_eq!(s.used_memory(), used_hot, "promote restores the exact weight");
    assert_eq!(s.map.get(b"k".as_slice()).unwrap().weight(), w_hot);
    assert_eq!(s.tier_stats().cold_bytes, 0);
}

// ---- sampler ---------------------------------------------------------

#[test]
fn demotion_sampler_skips_cold_and_non_spillable() {
    let (mut s, _d) = tiered("tier-sampler", 1); // budget 1 byte → always over watermark
    // Non-spillable population only: Int, small Str, list, set.
    s.set(b"int", b"42".to_vec(), None, false, false);
    s.set(b"small", b"tiny".to_vec(), None, false, false);
    s.lpush(b"list", &[&[b'x'; 200][..]]).unwrap();
    s.sadd(b"set", &[&[b'y'; 200][..]]).unwrap();
    assert_eq!(s.try_demote_after_write(), 0, "nothing spillable ⇒ no demotion");
    assert_eq!(s.tier_stats().demotions_total, 0);

    // One spillable key: demoted once, then (cold) never re-picked.
    s.set(b"bulk", vec![b'b'; 4096], None, false, false);
    assert_eq!(s.try_demote_after_write(), 1);
    assert!(is_cold(&s, b"bulk"));
    assert_eq!(s.try_demote_after_write(), 0, "cold keys are not candidates");
    assert_eq!(s.tier_stats().demotions_total, 1);
}

#[test]
fn a_single_write_spills_at_most_one_batch() {
    let (mut s, _d) = tiered("tier-batch", 1); // permanently over watermark
    for i in 0..100u32 {
        // The bare Store's SET does not run the demotion hook (that is
        // the serving layers' glue) — so one explicit call below sees
        // all 100 candidates at once.
        let key = format!("k{i:03}").into_bytes();
        s.set(&key, vec![b'v'; 1024], None, false, false);
    }
    let already = s.tier_stats().demotions_total;
    assert_eq!(already, 0);
    let demoted = s.try_demote_after_write();
    assert!(demoted <= 32, "B3: one call spills at most one batch, got {demoted}");
    assert_eq!(s.tier_stats().demotions_total, already + demoted as u64);
}

// ---- rename + compaction --------------------------------------------

#[test]
fn renamed_cold_key_survives_compaction() {
    let (mut s, _d) = tiered("tier-rename-compact", u64::MAX);
    let big = vec![b'r'; 3000];
    s.set(b"old", big.clone(), None, false, false);
    // Churn so the stub's file seals with mostly-dead bytes.
    for i in 0..64u32 {
        let key = format!("churn{i}").into_bytes();
        s.set(&key, vec![b'c'; 5000], None, false, false);
        s.debug_force_demote(&key);
    }
    assert!(s.debug_force_demote(b"old"));
    assert_eq!(s.rename(b"old", b"new", false), crate::RenameOutcome::Renamed);
    assert!(is_cold(&s, b"new"), "RENAME moves the stub without a read");
    // Kill the churn keys → their records die; force a compaction pass
    // by rotating (drop enough that ratios fall) and demote-batching.
    for i in 0..64u32 {
        s.del(&[format!("churn{i}").as_bytes()]);
    }
    // Direct trigger: run the compaction path via a fresh demote batch.
    s.set(b"trigger", vec![b't'; 5000], None, false, false);
    s.debug_force_demote(b"trigger");
    s.tier_force_compact_for_tests();
    assert_eq!(s.get(b"new").unwrap().unwrap().as_ref(), big.as_slice(), "record must survive rename + compaction");
}

#[test]
fn flushall_clears_the_cold_tier() {
    let (mut s, _d) = tiered("tier-flush", u64::MAX);
    s.set(b"k", vec![b'f'; 2048], None, false, false);
    assert!(s.debug_force_demote(b"k"));
    assert_eq!(s.tier_stats().cold_keys, 1);
    s.flushall();
    assert_eq!(s.dbsize(), 0);
    assert_eq!(s.tier_stats().cold_keys, 0);
    assert_eq!(s.tier_stats().cold_bytes, 0);
    assert_eq!(s.get(b"k").unwrap(), None);
}

// ---- T4: view pinning + serialization-side materialization ----------

#[test]
fn materialize_cold_returns_hot_twin_without_promotion() {
    let (mut s, _d) = tiered("tier-materialize", u64::MAX);
    s.set(b"bulk", vec![b'm'; 3000], None, false, false);
    s.hset(b"row", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
    assert!(s.debug_force_demote(b"bulk"));
    assert!(s.debug_force_demote(b"row"));

    let bulk = s.map.get(b"bulk".as_slice()).map(|e| e.value.clone()).unwrap();
    let row = s.map.get(b"row".as_slice()).map(|e| e.value.clone()).unwrap();
    match s.materialize_cold(b"bulk", &bulk).expect("cold materializes") {
        Value::ArcBulk(a) => assert_eq!(&a[..], &[b'm'; 3000][..]),
        other => panic!("unexpected variant {:?}", other.type_name()),
    }
    match s.materialize_cold(b"row", &row).expect("cold materializes") {
        Value::Hash(h) => assert_eq!(h.get(b"f".as_slice()).unwrap().as_slice(), b"v"),
        other => panic!("unexpected variant {:?}", other.type_name()),
    }
    // A hot value passes through as None (caller uses it verbatim).
    assert!(s.materialize_cold(b"i", &Value::Int(7)).is_none());
    // Peek only: nothing promoted, both stubs still cold.
    assert_eq!(s.tier_stats().promotions_total, 0);
    assert!(is_cold(&s, b"bulk") && is_cold(&s, b"row"));
}

/// The T4 pin proof: a snapshot view captured from a tiered store keeps
/// every vlog file alive across compaction — a file retired (and
/// scheduled for unlink) after the freeze still serves the view's
/// stubs, and only the view's last pin dropping deletes it.
#[test]
fn snapshot_view_pins_survive_file_retirement() {
    let (mut s, d) = tiered("tier-view-pins", u64::MAX);
    // Small rotate threshold so a handful of demotes seal real files
    // (enable_tiering hardcodes the production 256 MiB default).
    s.tier.as_mut().unwrap().vlog = kevy_vlog::Vlog::open(d.path(), 4096).unwrap();

    let frozen = noise(3000);
    s.set(b"pinned", frozen.clone(), None, false, false);
    assert!(s.debug_force_demote(b"pinned")); // → file 0
    s.set(b"filler", noise(3000), None, false, false);
    assert!(s.debug_force_demote(b"filler")); // → file 0 (now past rotate)
    s.set(b"other", noise(3000), None, false, false);
    assert!(s.debug_force_demote(b"other")); // rotates → file 1

    let view = s.collect_snapshot();

    // Kill file 0's records and retire it: the view is now the only
    // holder of that file.
    s.del(&[b"pinned".as_slice(), b"filler".as_slice()]);
    s.tier_force_compact_for_tests();
    let file0 = d.path().join("vlog-00000000.dat");
    assert!(file0.exists(), "a pinned retired file must not be unlinked");

    // The view still materializes the frozen instant from the retired
    // file — no store involvement, no promotion.
    let mut seen = false;
    view.each(|k, v, _| {
        if k == b"pinned" {
            assert!(matches!(v, Value::Cold(_)), "the view froze the stub");
            match view.materialize_cold(k, v).expect("stub materializes via pins") {
                Value::ArcBulk(a) => assert_eq!(&a[..], frozen.as_slice()),
                other => panic!("unexpected variant {:?}", other.type_name()),
            }
            seen = true;
        }
    });
    assert!(seen, "the frozen entry must be in the view");
    assert_eq!(s.tier_stats().promotions_total, 0);

    // Dropping the last pin deletes the retired file.
    drop(view);
    assert!(!file0.exists(), "the last pin dropping unlinks the retired file");
}

#[test]
fn del_and_overwrite_credit_dead_bytes() {
    let (mut s, _d) = tiered("tier-dead", u64::MAX);
    s.set(b"a", vec![b'a'; 1024], None, false, false);
    s.set(b"b", vec![b'b'; 1024], None, false, false);
    assert!(s.debug_force_demote(b"a"));
    assert!(s.debug_force_demote(b"b"));
    assert_eq!(s.del(&[b"a".as_slice()]), 1, "DEL counts the cold key");
    // Overwrite-SET on the raw fast path finds the stub Occupied.
    s.set(b"b", b"hot".to_vec(), None, false, false);
    assert_eq!(s.get(b"b").unwrap().unwrap().as_ref(), b"hot");
    let st = s.tier_stats();
    assert_eq!(st.cold_keys, 0);
    assert_eq!(st.cold_bytes, 0);
}

// ---- T5: unified budget arithmetic + stub accounting ----------------

const OVERHEAD: u64 = crate::value::ENTRY_OVERHEAD;

#[test]
fn t5_effective_target_subtracts_reserved_and_stub() {
    let budget = 1_000_000u64;
    let (mut s, _d) = tiered("tier-t5-target", budget);
    let wm = budget * 19 / 20;
    assert_eq!(s.tier_stats().effective_target, wm, "fresh tier: no floors");
    s.set_tier_reserved(100_000);
    assert_eq!(s.tier_stats().effective_target, wm - 100_000);
    // A demotion grows stub_bytes, which lowers the target further.
    s.set(b"k", vec![b'x'; 4096], None, false, false);
    assert!(s.debug_force_demote(b"k"));
    let st = s.tier_stats();
    assert_eq!(st.stub_bytes, OVERHEAD, "short key: stub = ENTRY_OVERHEAD only");
    assert_eq!(st.effective_target, wm - 100_000 - OVERHEAD);
}

#[test]
fn t5_saturated_target_is_zero_and_visible_not_silent() {
    let budget = 1_000u64;
    let (mut s, _d) = tiered("tier-t5-sat", budget);
    s.set_tier_reserved(budget); // floor alone exceeds the watermark
    let st = s.tier_stats();
    assert_eq!(st.effective_target, 0, "saturation must surface as 0 in the gauges");
    assert!(s.tier_index_floor_blocked(0), "IDX floor predicate sees the same state");
    // Untiered stores never block.
    let mut plain = Store::new();
    plain.set_tier_reserved(u64::MAX);
    assert!(!plain.tier_index_floor_blocked(u64::MAX));
}

#[test]
fn t5_reserved_pressure_triggers_demotion() {
    // 64 KiB budget; ~20 KiB hot — comfortably under the plain
    // watermark, but a 50 KiB index floor pushes the unified target
    // below the hot set, so the tick demotes.
    let budget = 64 * 1024u64;
    let (mut s, _d) = tiered("tier-t5-pressure", budget);
    for i in 0..10u32 {
        s.set(format!("k{i}").as_bytes(), vec![b'v'; 2048], None, false, false);
    }
    assert_eq!(s.demote_step(), 0, "under the plain watermark: nothing to do");
    s.set_tier_reserved(50 * 1024);
    assert!(s.demote_step() > 0, "the reserved floor must create demand");
}

#[test]
fn t5_stub_bytes_exact_across_demote_promote_del_rename_flush() {
    let (mut s, _d) = tiered("tier-t5-stub", u64::MAX);
    let long_key = vec![b'L'; 30]; // > 22-byte inline boundary → 30 heap bytes
    s.set(b"short", vec![b'a'; 2048], None, false, false);
    s.set(&long_key, vec![b'b'; 2048], None, false, false);
    assert!(s.debug_force_demote(b"short"));
    assert!(s.debug_force_demote(&long_key));
    let st = s.tier_stats();
    assert_eq!(st.cold_keys, 2);
    assert_eq!(st.stub_bytes, (OVERHEAD) + (OVERHEAD + 30), "96 + key heap each");
    assert!(st.cold_bytes > 0);

    // RENAME short → a long name: stub cost re-accounts for the key.
    let long_dst = vec![b'D'; 40];
    assert!(matches!(s.rename(b"short", &long_dst, false), crate::RenameOutcome::Renamed));
    assert_eq!(s.tier_stats().stub_bytes, (OVERHEAD + 40) + (OVERHEAD + 30));

    // Promote (two reads: serve, then install) releases the stub cost.
    assert_eq!(s.get(&long_key).unwrap().unwrap().len(), 2048);
    assert_eq!(s.get(&long_key).unwrap().unwrap().len(), 2048);
    assert!(!is_cold(&s, &long_key), "second touch promotes");
    let st = s.tier_stats();
    assert_eq!(st.stub_bytes, OVERHEAD + 40);
    assert_eq!(st.cold_keys, 1);

    // DEL of the remaining cold key zeroes both gauges.
    assert_eq!(s.del(&[long_dst.as_slice()]), 1);
    let st = s.tier_stats();
    assert_eq!((st.stub_bytes, st.cold_keys, st.cold_bytes), (0, 0, 0));

    // FLUSHALL from a re-demoted state zeroes in one stroke.
    s.set(b"again", vec![b'c'; 2048], None, false, false);
    assert!(s.debug_force_demote(b"again"));
    assert!(s.tier_stats().stub_bytes > 0);
    s.flushall();
    let st = s.tier_stats();
    assert_eq!((st.stub_bytes, st.cold_keys, st.cold_bytes), (0, 0, 0));
}

#[test]
fn t5_live_budget_update_does_not_disturb_the_vlog() {
    let (mut s, _d) = tiered("tier-t5-budget", u64::MAX);
    s.set(b"cold", vec![b'z'; 4096], None, false, false);
    assert!(s.debug_force_demote(b"cold"));
    let before = s.tier_stats();
    s.set_tier_budget(123_456);
    let after = s.tier_stats();
    assert_eq!(after.budget, 123_456);
    assert_eq!(
        (after.vlog_files, after.vlog_bytes, after.vlog_live_bytes, after.vlog_epoch),
        (before.vlog_files, before.vlog_bytes, before.vlog_live_bytes, before.vlog_epoch),
        "a budget update must not touch the vlog"
    );
    assert_eq!((after.cold_keys, after.stub_bytes), (before.cold_keys, before.stub_bytes));
    // The cold value still reads back through the new budget.
    assert_eq!(s.get(b"cold").unwrap().unwrap().len(), 4096);
}

#[test]
fn t5_stats_carry_vlog_gauges() {
    let (mut s, _d) = tiered("tier-t5-vlog", u64::MAX);
    s.set(b"v", noise(1024), None, false, false);
    assert!(s.debug_force_demote(b"v"));
    let st = s.tier_stats();
    assert_eq!(st.vlog_files, 1);
    assert!(st.vlog_bytes > 1024, "the record is on disk");
    assert_eq!(st.vlog_live_bytes, st.vlog_bytes, "nothing dead yet");
    assert_eq!(st.vlog_epoch, 0);
}

// ---- max_spill cap (RFC §7: embedded bounds cold-read lock hold) ----

#[test]
fn max_spill_caps_the_largest_demotable_value() {
    let (mut s, _d) = tiered("tier-maxspill", 1 << 30);
    s.set_tier_max_spill(1024);
    s.set(b"big", vec![b'a'; 4096], None, false, false); // over the cap
    s.set(b"small", vec![b'b'; 300], None, false, false); // under the cap
    assert!(!s.debug_force_demote(b"big"), "over-cap value must stay hot");
    assert!(!is_cold(&s, b"big"));
    assert!(s.debug_force_demote(b"small"), "under-cap value demotes");
    assert!(is_cold(&s, b"small"));
    // Lifting the cap (0 = unlimited) makes the big value eligible.
    s.set_tier_max_spill(0);
    assert!(s.debug_force_demote(b"big"));
    assert!(is_cold(&s, b"big"));
}

/// v4.1-V5: a tick whose batch moves nothing while over target backs
/// off exponentially — "idempotent is not convergent"; the old
/// behavior re-walked the sample window every tick forever (mailrs
/// measured it as 300-500× idle CPU with tiering on).
#[test]
fn a_dry_tick_backs_off_exponentially_to_the_ceiling() {
    let (mut s, _d) = tiered("tier-backoff", 1); // budget 1 ⇒ always over
    s.set(b"int", b"42".to_vec(), None, false, false); // nothing spillable
    let skip = |s: &Store| {
        let t = s.tier.as_ref().unwrap();
        (t.tick_skip, t.tick_wait)
    };
    assert_eq!(s.demote_step(), 0, "dry batch");
    assert_eq!(skip(&s), (1, 1), "first dry tick arms a 1-tick skip");
    assert_eq!(s.demote_step(), 0, "skipped tick: decrement only");
    assert_eq!(skip(&s), (1, 0));
    assert_eq!(s.demote_step(), 0, "dry again");
    assert_eq!(skip(&s), (2, 2), "the skip doubles");
    for _ in 0..2000 {
        let _ = s.demote_step();
    }
    assert_eq!(
        skip(&s).0,
        crate::tier_demote::BACKOFF_CEILING_TICKS,
        "the skip is capped, never unbounded"
    );
}

/// The write path never waits out a backoff window — it samples on
/// every over-target commit, and its progress wakes the tick sampler.
#[test]
fn a_write_path_demotion_wakes_the_backed_off_tick() {
    let (mut s, _d) = tiered("tier-backoff-reset", 1);
    s.set(b"int", b"42".to_vec(), None, false, false);
    for _ in 0..10 {
        let _ = s.demote_step();
    }
    assert!(s.tier.as_ref().unwrap().tick_skip >= 2, "backed off");
    s.set(b"bulk", vec![b'b'; 4096], None, false, false);
    assert_eq!(s.try_demote_after_write(), 1, "write path samples during the window");
    let t = s.tier.as_ref().unwrap();
    assert_eq!((t.tick_skip, t.tick_wait), (0, 0), "progress resets the tick backoff");
}

/// `effective_target == 0` (the index floor alone exceeds the budget)
/// enters the same idle path: the tick can never make progress there,
/// and before v4.1-V5 it was guaranteed one full sample walk per tick
/// forever.
#[test]
fn a_zero_effective_target_backs_off_like_any_dry_tick() {
    let (mut s, _d) = tiered("tier-backoff-floor", 1 << 20);
    s.set_tier_reserved(1 << 30); // floor >> budget ⇒ effective_target 0
    s.set(b"bulk", vec![b'b'; 4096], None, false, false);
    assert_eq!(s.demote_step(), 1, "the one spillable value still demotes");
    assert_eq!(s.demote_step(), 0, "then the tick runs dry");
    assert_eq!(s.tier.as_ref().unwrap().tick_skip, 1, "and backs off");
}

/// `ENTRY_OVERHEAD` is charged per key and is the denominator of every
/// capacity ratio the tier can claim — a 256 B workload reaches 2.65x
/// because 256 B of value sits against this — so what it stands for is
/// worth pinning, not just that it is "conservative".
///
/// It is NOT padding over the struct sizes. The keyspace is an
/// open-addressing table that grows by doubling at a 7/8 max load, so a
/// live entry's real cost is its slot divided by wherever the table
/// currently sits in that cycle: cheapest just before a growth, nearly
/// 2x that just after one. 96 sits inside that band, low-ish — which is
/// the right direction for a bound that must not flatter the engine on
/// a freshly grown table.
///
/// If someone shrinks `Entry` or the key cell, this test says by how
/// much the constant may follow.
#[test]
fn entry_overhead_stands_for_a_slot_in_a_growing_table() {
    let entry = std::mem::size_of::<crate::entry::Entry>();
    let key = std::mem::size_of::<kevy_bytes::SmallBytes>();
    let slot = key + entry + 1; // + one control byte per slot
    // Occupancy runs from 7/16 (just after doubling) to 7/8 (at the
    // growth threshold), so amortised cost per LIVE entry is slot/load.
    let at_full = slot as f64 / (7.0 / 8.0);
    let at_fresh = slot as f64 / (7.0 / 16.0);
    let charged = crate::value::ENTRY_OVERHEAD as f64;
    eprintln!(
        "Entry={entry}B key-cell={key}B slot={slot}B -> amortised \
         {at_full:.0}B (full) .. {at_fresh:.0}B (just grown); charged {charged:.0}B"
    );
    assert!(
        charged >= at_full,
        "ENTRY_OVERHEAD {charged} under-states even a full table ({at_full:.0}B) — \
         used_memory would stop being a bound"
    );
    assert!(
        charged <= at_fresh,
        "ENTRY_OVERHEAD {charged} exceeds the worst case ({at_fresh:.0}B) — \
         the budget would be spent on accounting that no table pays"
    );
}
