//! The reaper tick packs index leaves: rows written in scattered order
//! leave leaves about 70% full, and ticking a manual-reaper store packs
//! them without touching what the index answers.

#![cfg(feature = "index")]

use kevy_embedded::{Config, IndexKind, IndexValType, IndexValue, Store};

#[test]
fn the_tick_packs_index_leaves_and_the_index_answers_the_same() {
    let s = Store::open(Config::default().with_shards(1).with_ttl_reaper_manual()).unwrap();
    s.idx_create(b"age", b"user:", b"age", IndexValType::I64, IndexKind::Range).unwrap();
    for i in 0..20_000u32 {
        let age = (i.wrapping_mul(2_654_435_761) % 100_000).to_string();
        s.hset(format!("user:{i}").as_bytes(), &[(b"age".as_slice(), age.as_bytes())]).unwrap();
    }
    let count = |s: &Store| s.idx_count(b"age", &IndexValue::I64(0), &IndexValue::I64(49_999));
    let (written, below) = (s.idx_stats(b"age").unwrap(), count(&s).unwrap());
    for _ in 0..50 {
        s.tick();
    }
    let packed = s.idx_stats(b"age").unwrap();
    assert_eq!(packed.entries, 20_000);
    assert_eq!(count(&s).unwrap(), below);
    assert!(
        packed.approx_bytes * 10 < written.approx_bytes * 8,
        "{} bytes written, {} after ticking",
        written.approx_bytes,
        packed.approx_bytes
    );
}
