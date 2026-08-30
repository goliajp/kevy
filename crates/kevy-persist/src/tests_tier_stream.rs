//! T4 persistence-streaming suite (capacity arc, RFC §1 D1 / §2
//! B10-B11 unit halves): cold values stream from the (pinned) vlog into
//! every serializer — AOF rewrite, snapshot write, view-based ship —
//! with zero promotion; snapshot LOAD demotes inline when the target
//! store's dataset exceeds its tier budget.

use std::io::Cursor;
use std::time::Duration;

use crate::tests_rewrite::apply_for_test;
use crate::{
    AofFormat, dump_aof, dump_store_to_buf, load_snapshot, load_snapshot_from, replay_aof,
    save_snapshot, write_snapshot_to,
};
use kevy_store::Store;

fn tiered(name: &str, budget: u64) -> (Store, kevy_tmpdir::TmpDir) {
    let d = kevy_tmpdir::TmpDir::new(name);
    let mut s = Store::new();
    s.enable_tiering(&d.path().join("tier"), budget).unwrap();
    (s, d)
}

/// The dump of a tiered store with cold keys is byte-identical to the
/// dump of the same (fully hot) store — the codec decode is reachable
/// from the persist serializers and emits exactly what the hot value
/// would have. Bulk values only: their demote/decode round trip lands
/// on the same `Value` variant, so even the bytes match 1:1.
#[test]
fn rewrite_of_cold_bulk_values_is_byte_identical_to_hot() {
    let mut hot = Store::new();
    let (mut cold, _d) = tiered("tier-stream-ident", u64::MAX);
    for i in 0..5u8 {
        let key = [b'k', b'0' + i];
        let val = vec![b'a' + i; 4096];
        hot.set(&key, val.clone(), None, false, false);
        cold.set(&key, val, None, false, false);
    }
    assert!(cold.debug_force_demote(b"k1"));
    assert!(cold.debug_force_demote(b"k3"));

    let (buf_hot, keys_hot) = dump_store_to_buf(&hot, AofFormat::V2);
    let (buf_cold, keys_cold) = dump_store_to_buf(&cold, AofFormat::V2);
    assert_eq!(keys_hot, keys_cold);
    assert_eq!(buf_hot, buf_cold, "cold stream must emit the hot bytes");

    // NEVER promoted during serialization; the stubs are still cold.
    let st = cold.tier_stats();
    assert_eq!(st.promotions_total, 0, "serialization must not promote");
    assert_eq!(st.demotions_total, 2);
    assert_eq!(st.cold_keys, 2);
}

/// Full rewrite → replay round trip on a tiered store: cold bulk +
/// cold hash (incl. a field TTL — the co-serialized sidecar) come back
/// byte-correct, and the dump itself moves no tier counters.
#[test]
fn rewrite_roundtrip_restores_cold_hashes_and_field_ttls() {
    let (mut s, d) = tiered("tier-stream-roundtrip", u64::MAX);
    s.set(b"bulk", vec![b'x'; 5000], None, false, false);
    s.set(b"ttl'd", vec![b'y'; 2048], Some(Duration::from_secs(600)), false, false);
    s.hset(
        b"row",
        &[(b"name".as_slice(), b"ada".as_slice()), (b"blob".as_slice(), &[0u8, 255, 7][..])],
    )
    .unwrap();
    let deadline = kevy_store::now_unix_ms() + 60_000;
    s.load_hash_field_ttl(b"row", b"name", deadline);
    for k in [b"bulk".as_slice(), b"ttl'd", b"row"] {
        assert!(s.debug_force_demote(k), "warm spillable key must demote");
    }

    let before = s.tier_stats();
    let aof = d.path().join("rewrite.aof");
    dump_aof(&aof, &s).unwrap();
    let after = s.tier_stats();
    assert_eq!(after.promotions_total, before.promotions_total, "rewrite must not promote");
    assert_eq!(after.demotions_total, before.demotions_total);
    assert_eq!(after.cold_keys, before.cold_keys, "stubs must stay cold");

    let mut dst = Store::new();
    replay_aof(&aof, |args| apply_for_test(&mut dst, &args)).unwrap();
    assert_eq!(dst.get(b"bulk").unwrap().unwrap().as_ref(), &[b'x'; 5000][..]);
    assert_eq!(dst.get(b"ttl'd").unwrap().unwrap().as_ref(), &[b'y'; 2048][..]);
    assert_eq!(dst.hget(b"row", b"name").unwrap().unwrap(), b"ada");
    assert_eq!(dst.hget(b"row", b"blob").unwrap().unwrap(), &[0u8, 255, 7][..]);
    let mut fttl: Vec<(Vec<u8>, Vec<u8>, u64)> = Vec::new();
    dst.hash_ttl_each(|k, f, dl| fttl.push((k.to_vec(), f.to_vec(), dl)));
    assert_eq!(fttl, vec![(b"row".to_vec(), b"name".to_vec(), deadline)]);
}

/// Snapshot write → load round trip with cold stubs in the source.
#[test]
fn snapshot_of_cold_store_round_trips() {
    let (mut s, d) = tiered("tier-stream-snap", u64::MAX);
    s.set(b"bulk", vec![b'z'; 4096], None, false, false);
    s.hset(b"row", &[(b"f1".as_slice(), b"v1".as_slice())]).unwrap();
    assert!(s.debug_force_demote(b"bulk"));
    assert!(s.debug_force_demote(b"row"));

    let snap = d.path().join("dump.rdb");
    save_snapshot(&s, &snap).unwrap();
    assert_eq!(s.tier_stats().promotions_total, 0, "snapshot must not promote");
    assert_eq!(s.tier_stats().cold_keys, 2);

    let mut dst = Store::new();
    load_snapshot(&mut dst, &snap).unwrap();
    assert_eq!(dst.get(b"bulk").unwrap().unwrap().as_ref(), &[b'z'; 4096][..]);
    assert_eq!(dst.hget(b"row", b"f1").unwrap().unwrap(), b"v1");
}

/// The replication-ship mechanism at unit level: a frozen view carries
/// the vlog pins, so serializing it AFTER the store has moved on
/// (deleted + overwritten the cold key) still yields the frozen
/// instant's bytes — the pins keep the record readable.
#[test]
fn pinned_view_serializes_the_frozen_instant_after_store_moves_on() {
    let (mut s, _d) = tiered("tier-stream-view", u64::MAX);
    s.set(b"cold", vec![b'q'; 4096], None, false, false);
    assert!(s.debug_force_demote(b"cold"));
    let view = s.collect_snapshot();

    // The store moves on: the record is dead in the live map.
    s.del(&[b"cold".as_slice()]);
    s.set(b"cold", b"new".to_vec(), None, false, false);

    let mut buf = Vec::new();
    write_snapshot_to(&view, &mut buf).unwrap();
    let mut dst = Store::new();
    load_snapshot_from(&mut dst, Cursor::new(&buf)).unwrap();
    assert_eq!(
        dst.get(b"cold").unwrap().unwrap().as_ref(),
        &[b'q'; 4096][..],
        "the view must serialize its frozen instant, not the live state"
    );
}

/// B11 unit half: loading a snapshot bigger than the target's tier
/// budget demotes inline — the load completes under the watermark with
/// the overflow spilled cold, and every value still reads back.
#[test]
fn snapshot_load_demotes_inline_when_over_budget() {
    let d = kevy_tmpdir::TmpDir::new("tier-stream-b11");
    let mut src = Store::new();
    for i in 0..1500u32 {
        let key = format!("k{i:04}").into_bytes();
        src.set(&key, vec![(i % 251) as u8; 1024], None, false, false);
    }
    let snap = d.path().join("big.rdb");
    save_snapshot(&src, &snap).unwrap();

    // Must sit above the stub floor (1500 × ~96 B ≈ 144 KB — the RFC's
    // ENTRY_OVERHEAD model) or the drain can never reach the watermark.
    let budget = 256 * 1024;
    let mut dst = Store::new();
    dst.enable_tiering(&d.path().join("tier"), budget).unwrap();
    load_snapshot(&mut dst, &snap).unwrap();

    assert!(
        dst.used_memory() <= budget * 19 / 20,
        "load must end under the demote watermark: {} > {}",
        dst.used_memory(),
        budget * 19 / 20
    );
    let st = dst.tier_stats();
    assert!(st.demotions_total > 0, "the overflow must have spilled");
    assert!(st.cold_keys > 0);
    for i in [0u32, 749, 1499] {
        let key = format!("k{i:04}").into_bytes();
        let got = dst.get(&key).unwrap().unwrap();
        assert_eq!(got.as_ref(), &vec![(i % 251) as u8; 1024][..], "key {i}");
    }
}
