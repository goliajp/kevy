//! What the tier budget holds `used_memory` to: every byte charged once,
//! the hot set filling the room the budget leaves it.

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
