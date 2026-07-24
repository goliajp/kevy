//! Tiering unit suite (capacity arc T3): codec round trips, the
//! demote/promote in-place contract (value + TTL + lru_clock + hfttl +
//! WATCH + events), WRONGTYPE-with-zero-preads, exact accounting, the
//! sampler's cold/non-spillable skips, the spill-batch bound (B3),
//! rename-then-compact survival, and the FLUSHALL wipe.

use crate::value::{COLD_TAG_HASH, COLD_TAG_STRING, Value};
use crate::{Store, StoreError, tier_codec};
use core::time::Duration;

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
