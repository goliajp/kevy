//! What the tier budget holds `used_memory` to: every byte charged once,
//! the hot set filling the room the budget leaves it.

#![allow(clippy::unwrap_used, clippy::panic)]

use crate::Store;

fn tiered(name: &str, budget: u64) -> (Store, kevy_tmpdir::TmpDir) {
    let d = kevy_tmpdir::TmpDir::new(name);
    let mut s = Store::new();
    s.enable_tiering(d.path(), budget).unwrap();
    (s, d)
}

#[test]
fn demotion_stops_at_the_watermark_not_below_it_by_the_stubs() {
    let budget = 1 << 20;
    let (mut s, _d) = tiered("tier-budget-once", budget);
    for i in 0..1000u32 {
        s.set(format!("k{i}").as_bytes(), vec![b'v'; 4096], None, crate::SetCondition::Always);
    }
    s.demote_to_watermark();
    let st = s.tier_stats();
    assert!(st.cold_keys > 700, "most of 4 MB went cold on a 1 MiB budget: {}", st.cold_keys);
    let wm = budget * 19 / 20;
    assert!(s.used_memory() <= wm);
    // the cold keys are in used_memory already (their table slots and key
    // bytes); charging them again as a floor left a gap of stub_bytes
    // between the hot set and the watermark
    let gap = wm - s.used_memory();
    assert!(gap < 4096 + 128, "one value short of the watermark at most, gap {gap} B");
}

#[test]
fn the_index_floor_counts_what_stays_when_everything_is_cold() {
    let budget = 1 << 20;
    let (mut s, _d) = tiered("tier-budget-floor", budget);
    for i in 0..5000u32 {
        s.set(format!("k{i}").as_bytes(), vec![b'v'; 256], None, crate::SetCondition::Always);
    }
    let wm = budget * 19 / 20;
    let table = s.map.footprint() as u64;
    assert!(table > 0 && table < wm);
    // an index that fits beside the hot values, but not beside the table
    // that stays however cold the values get, is refused
    assert!(!s.tier_index_floor_blocked(wm - table - 1));
    assert!(s.tier_index_floor_blocked(wm - table));
}

#[test]
fn tick_demotion_keeps_going_with_no_access_to_move_its_sampler() {
    // a backfill reads rows without touching them, so the access clock
    // stands still while the index floor rises; the tick must still sweep
    // the table instead of re-walking one window that has gone cold
    let budget = 64 << 20;
    let (mut s, _d) = tiered("tier-budget-sweep", budget);
    for i in 0..40_000u32 {
        s.set(format!("k{i}").as_bytes(), vec![b'v'; 512], None, crate::SetCondition::Always);
    }
    assert_eq!(s.tier_stats().cold_keys, 0);
    let wm = budget * 19 / 20;
    let floor = wm - s.used_memory() / 4;
    s.set_tier_reserved(floor);
    for _ in 0..10_000 {
        if s.used_memory() + floor <= wm {
            break;
        }
        s.demote_step();
    }
    assert!(
        s.used_memory() + floor <= wm,
        "demotion stalled at {} cold keys, used {} over a target of {}",
        s.tier_stats().cold_keys,
        s.used_memory(),
        wm - floor
    );
}

#[test]
fn over_the_target_with_no_keys_left_demotion_stops_with_nothing_to_move() {
    let (mut s, _d) = tiered("tier-budget-empty", 1 << 20);
    s.set(b"k", vec![b'v'; 4096], None, crate::SetCondition::Always);
    s.flushall();
    // the emptied table keeps its charge, and the overhead leaves no room for it
    assert!(s.used_memory() > 0);
    s.set_tier_overhead(1 << 30);
    assert_eq!(s.demote_to_watermark(), 0);
    assert_eq!(s.tier_stats().cold_keys, 0);
}
