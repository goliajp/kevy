//! T6 peek suite: the no-promote row/page peeks and the bulk-read
//! scope. Asserts the D1/D3 counter contract — preads == cold ROWS
//! (never rows × fields), one batch submission per page — and that no
//! peek ever promotes or advances the 2nd-touch gate.

use crate::value::Value;
use crate::{ColdBatchReader, ColdRead, Store, StoreError, SyncColdRead};

fn tiered(name: &str) -> (Store, kevy_tmpdir::TmpDir) {
    let d = kevy_tmpdir::TmpDir::new(name);
    let mut s = Store::new();
    // Budget big enough that nothing demotes by watermark — every
    // demotion in this suite is explicit (`debug_force_demote`).
    s.enable_tiering(d.path(), 1 << 30).unwrap();
    (s, d)
}

fn is_cold(s: &Store, key: &[u8]) -> bool {
    matches!(s.map.get(key).map(|e| &e.value), Some(Value::Cold(_)))
}

/// A row big enough to be heap-backed (spillable class).
fn put_row(s: &mut Store, key: &[u8]) {
    let a = vec![b'a'; 40];
    let b = vec![b'b'; 40];
    s.hset(
        key,
        &[
            (b"a".as_slice(), a.as_slice()),
            (b"b".as_slice(), b.as_slice()),
            (b"c".as_slice(), b"tiny".as_slice()),
            (b"pad-field-name-over-inline".as_slice(), b"x".as_slice()),
        ],
    )
    .unwrap();
}

// ---- peek_hash_fields ------------------------------------------------

#[test]
fn peek_fields_hot_cold_missing_wrongtype() {
    let (mut s, _d) = tiered("peek-fields");
    put_row(&mut s, b"hot");
    put_row(&mut s, b"cold");
    s.set(b"str", vec![b's'; 100], None, false, false);
    assert!(s.debug_force_demote(b"cold"));
    assert!(s.debug_force_demote(b"str"));

    let fields: &[&[u8]] = &[b"a", b"missing-field", b"c"];
    // Hot row: hmget-shaped.
    let hot = s.peek_hash_fields(b"hot", fields).unwrap().unwrap();
    assert_eq!(hot[0].as_deref(), Some(&vec![b'a'; 40][..]));
    assert_eq!(hot[1], None);
    assert_eq!(hot[2].as_deref(), Some(&b"tiny"[..]));
    // Cold row: same values, ONE record read.
    let before = s.tier_stats();
    let cold = s.peek_hash_fields(b"cold", fields).unwrap().unwrap();
    assert_eq!(cold, hot);
    let after = s.tier_stats();
    assert_eq!(after.peek_preads_total - before.peek_preads_total, 1, "3 fields = 1 pread");
    // Missing key.
    assert!(s.peek_hash_fields(b"nope", fields).unwrap().is_none());
    // Wrong type: cold string answers from the tag — zero preads.
    let before = s.tier_stats();
    assert!(matches!(s.peek_hash_fields(b"str", fields), Err(StoreError::WrongType)));
    assert_eq!(s.tier_stats().preads_total, before.preads_total, "WRONGTYPE pays no pread");
}

#[test]
fn peek_never_promotes_and_never_advances_the_gate() {
    let (mut s, _d) = tiered("peek-gate");
    put_row(&mut s, b"k");
    assert!(s.debug_force_demote(b"k"));

    // Many peeks: still cold, zero promotions, gate mark untouched.
    for _ in 0..5 {
        s.peek_hash_fields(b"k", &[b"a", b"b"]).unwrap().unwrap();
    }
    assert!(is_cold(&s, b"k"), "peek must not promote");
    assert_eq!(s.tier_stats().promotions_total, 0);
    // The 2nd-touch gate starts from scratch: the FIRST client read
    // serves without installing (still cold), the SECOND promotes.
    let _ = s.hget(b"k", b"a").unwrap();
    assert!(is_cold(&s, b"k"), "peeks must not count as the gate's first touch");
    let _ = s.hget(b"k", b"a").unwrap();
    assert!(!is_cold(&s, b"k"), "second client read promotes");
    assert_eq!(s.tier_stats().promotions_total, 1);
}

// ---- peek_hash_rows (the batched page) -------------------------------

#[test]
fn page_peek_mixed_rows_in_order_one_batch() {
    let (mut s, _d) = tiered("peek-page");
    for i in 0..6u8 {
        put_row(&mut s, format!("row:{i}").as_bytes());
    }
    s.set(b"strval", vec![b'v'; 100], None, false, false);
    for i in [1u8, 3, 4] {
        assert!(s.debug_force_demote(format!("row:{i}").as_bytes()));
    }
    assert!(s.debug_force_demote(b"strval"));

    let keys: Vec<&[u8]> =
        vec![b"row:0", b"row:1", b"row:2", b"row:3", b"missing", b"strval", b"row:4", b"row:5"];
    let fields: &[&[u8]] = &[b"a", b"c", b"nope"];
    let before = s.tier_stats();
    let rows = s.peek_hash_rows(&keys, fields, &mut SyncColdRead);
    let after = s.tier_stats();

    assert_eq!(rows.len(), keys.len());
    // Cold rows byte-equal their hot twins, in the original order.
    let expect = s.peek_hash_fields(b"row:0", fields).unwrap().unwrap();
    for i in [0usize, 1, 2, 3, 6, 7] {
        assert_eq!(rows[i].as_ref().unwrap().as_ref().unwrap(), &expect, "row {i}");
    }
    assert!(rows[4].as_ref().unwrap().is_none(), "missing key");
    assert!(matches!(rows[5], Err(StoreError::WrongType)), "cold string");
    // Counter contract: 3 cold rows = 3 preads (not 3 rows × 3 fields),
    // ONE batch submission, zero promotions, gate not advanced.
    assert_eq!(after.peek_preads_total - before.peek_preads_total, 3);
    assert_eq!(after.batch_submissions_total - before.batch_submissions_total, 1);
    assert_eq!(after.promotions_total, 0);
    for i in [1u8, 3, 4] {
        assert!(is_cold(&s, format!("row:{i}").as_bytes()));
    }
}

#[test]
fn page_peek_all_hot_makes_no_batch() {
    let (mut s, _d) = tiered("peek-page-hot");
    put_row(&mut s, b"h1");
    put_row(&mut s, b"h2");
    let rows = s.peek_hash_rows(&[b"h1", b"h2"], &[b"a"], &mut SyncColdRead);
    assert!(rows.iter().all(|r| r.as_ref().unwrap().is_some()));
    let ts = s.tier_stats();
    assert_eq!((ts.peek_preads_total, ts.batch_submissions_total), (0, 0));
}

/// A reader that records the offsets it was asked for — proves the
/// plan arrives `(file_id, offset)`-sorted — then delegates to the
/// sync loop.
struct RecordingReader(Vec<(u32, u64)>);

impl ColdBatchReader for RecordingReader {
    fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
        self.0 = reads.iter().map(|r| (r.vref.file_id, r.vref.offset)).collect();
        SyncColdRead.read_batch(reads)
    }
}

#[test]
fn page_peek_plan_is_sorted_by_file_and_offset() {
    let (mut s, _d) = tiered("peek-sorted");
    for i in 0..5u8 {
        put_row(&mut s, format!("r{i}").as_bytes());
        assert!(s.debug_force_demote(format!("r{i}").as_bytes()));
    }
    // Query in scrambled order; the plan must still be offset-sorted.
    let keys: Vec<&[u8]> = vec![b"r3", b"r0", b"r4", b"r2", b"r1"];
    let mut rec = RecordingReader(Vec::new());
    let rows = s.peek_hash_rows(&keys, &[b"a"], &mut rec);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r.as_ref().unwrap().is_some()));
    let sorted = {
        let mut v = rec.0.clone();
        v.sort_unstable();
        v
    };
    assert_eq!(rec.0, sorted, "cold batch must be (file_id, offset)-sorted");
    assert_eq!(rec.0.len(), 5);
}

// ---- peek_scope (whole-value bulk reads) -----------------------------

#[test]
fn peek_scope_reads_cold_without_promoting_or_marking() {
    let (mut s, _d) = tiered("peek-scope");
    put_row(&mut s, b"h");
    s.set(b"v", vec![b'z'; 200], None, false, false);
    let hot_hash = s.hgetall(b"h").unwrap();
    let hot_val = s.get(b"v").unwrap().map(|c| c.to_vec());
    assert!(s.debug_force_demote(b"h"));
    assert!(s.debug_force_demote(b"v"));

    let before = s.tier_stats();
    let (cold_hash, cold_val) = s.peek_scope(|s| {
        let h = s.hgetall(b"h").unwrap();
        let v = s.get(b"v").unwrap().map(|c| c.to_vec());
        (h, v)
    });
    assert_eq!(cold_hash, hot_hash, "digest-style hgetall equals the hot twin");
    assert_eq!(cold_val, hot_val);
    let after = s.tier_stats();
    assert_eq!(after.promotions_total, 0);
    assert_eq!(after.peek_preads_total - before.peek_preads_total, 2, "one pread per row");
    assert!(is_cold(&s, b"h") && is_cold(&s, b"v"));
    // Gate untouched: the 2-touch dance still takes two client reads.
    let _ = s.get(b"v").unwrap();
    assert!(is_cold(&s, b"v"));
    let _ = s.get(b"v").unwrap();
    assert!(!is_cold(&s, b"v"));
}

#[test]
fn peek_scope_restores_gate_behavior_after_the_scope() {
    let (mut s, _d) = tiered("peek-scope-restore");
    put_row(&mut s, b"k");
    assert!(s.debug_force_demote(b"k"));
    s.peek_scope(|s| {
        let _ = s.hget(b"k", b"a").unwrap();
    });
    // Outside the scope the normal gate applies again.
    let _ = s.hget(b"k", b"a").unwrap(); // first touch
    let _ = s.hget(b"k", b"a").unwrap(); // second → promote
    assert!(!is_cold(&s, b"k"));
    assert_eq!(s.tier_stats().promotions_total, 1);
}
