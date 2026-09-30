//! Keywords, small accessors and the switches a caller flips on a store.

use std::time::Duration;

use crate::{
    EvictionPolicy, HExpireCond, KeyspaceEvent, ScoreCompare, SetCondition, SmallBytes, Store,
    StoreError, StreamId, XClaimOpts, ZAggregate, ZaddFlags, hash_field_weight, value::Score,
};

#[test]
fn every_condition_and_aggregate_names_its_keyword() {
    let conds = [
        (HExpireCond::Always, None),
        (HExpireCond::Nx, Some("NX")),
        (HExpireCond::Xx, Some("XX")),
        (HExpireCond::Gt, Some("GT")),
        (HExpireCond::Lt, Some("LT")),
    ];
    for (c, kw) in conds {
        assert_eq!(c.keyword(), kw);
    }
    assert_eq!(ZAggregate::Sum.keyword(), "SUM");
    assert_eq!(ZAggregate::Min.keyword(), "MIN");
    assert_eq!(ZAggregate::Max.keyword(), "MAX");
    assert_eq!(KeyspaceEvent::New.name(), "new");
    assert_eq!(KeyspaceEvent::Expired.name(), "expired");
    assert_eq!(KeyspaceEvent::Evicted.name(), "evicted");
}

#[test]
fn zadd_flags_hand_back_the_condition_and_comparison_they_were_built_from() {
    let f = ZaddFlags::new(SetCondition::IfPresent, ScoreCompare::Greater).expect("legal");
    assert_eq!(f.condition(), SetCondition::IfPresent);
    assert_eq!(f.compare(), ScoreCompare::Greater);
    assert!(ZaddFlags::new(SetCondition::IfAbsent, ScoreCompare::Less).is_none());
}

#[test]
fn a_score_wraps_and_unwraps_its_float() {
    assert_eq!(Score::new(-2.5).value(), -2.5);
    // ordered totally: the two zeros are different scores
    assert!(Score::new(-0.0) < Score::new(0.0));
    assert_ne!(Score::new(0.0), Score::new(-0.0));
}

#[test]
fn a_hash_field_is_charged_what_the_allocator_holds_for_its_spilled_parts() {
    assert_eq!(hash_field_weight(&SmallBytes::from_slice(b"name"), 0), 0);
    assert_eq!(hash_field_weight(&SmallBytes::from_slice(&[b'f'; 64]), 0), 80);
    assert_eq!(hash_field_weight(&SmallBytes::from_slice(&[b'f'; 64]), 900), 80 + 912);
}

#[test]
fn a_stream_id_past_the_last_sequence_rolls_into_the_next_millisecond() {
    assert_eq!(StreamId::new(5, 1).next(), StreamId::new(5, 2));
    assert_eq!(StreamId::new(5, u64::MAX).next(), StreamId::new(6, 0));
    assert_eq!(StreamId::new(u64::MAX, u64::MAX).next(), StreamId::MAX);
}

#[test]
fn a_claim_with_an_idle_time_backdates_the_delivery() {
    let mut st = Store::new();
    super::grouped_stream_fixture(&mut st);
    let opts = XClaimOpts::default().with_idle_ms(500);
    st.xclaim(b"st", b"g", b"c2", &[StreamId::new(1, 1)], &opts, 5_000).unwrap();
    let g = st.stream_group_peek(b"st", b"g").unwrap();
    assert_eq!(g.pending_entry(StreamId::new(1, 1)).unwrap().delivery_time_ms, 4_500);
    let c2 = g.consumer(b"c2").expect("c2 reads");
    assert_eq!(c2.name(), b"c2");
    let mut names: Vec<&[u8]> = g.consumers().map(|(_, c)| c.name()).collect();
    names.sort_unstable();
    assert_eq!(names, [&b"c1"[..], b"c2"]);
}

#[test]
fn detached_entries_count_what_was_moved_out() {
    let mut s = Store::new();
    assert!(s.detach_entries().is_empty());
    s.set(b"a", b"1".to_vec(), None, SetCondition::Always);
    s.set(b"b", b"2".to_vec(), None, SetCondition::Always);
    let d = s.detach_entries();
    assert!(!d.is_empty());
    assert_eq!(d.len(), 2);
}

#[test]
fn a_memory_refusal_stops_writes_until_lifted() {
    let mut s = Store::new();
    s.set_memory_refusal(true);
    assert!(s.memory_refused());
    assert_eq!(s.precheck_for_write(), Err(StoreError::OutOfMemory));
    s.set_memory_refusal(false);
    assert_eq!(s.precheck_for_write(), Ok(()));
}

#[test]
fn over_the_limit_only_noeviction_refuses_up_front() {
    let mut s = Store::new();
    for i in 0..64u32 {
        s.set(format!("k{i}").as_bytes(), vec![b'v'; 200], None, SetCondition::Always);
    }
    s.set_max_memory(1, EvictionPolicy::AllKeysLru);
    assert_eq!(s.precheck_for_write(), Ok(()), "an evicting policy makes room after the write");
    s.set_max_memory(1, EvictionPolicy::NoEviction);
    assert_eq!(s.precheck_for_write(), Err(StoreError::OutOfMemory));
}

#[test]
fn turning_the_cached_clock_off_keeps_expiry_on_the_live_clock() {
    let mut s = Store::new();
    s.set_cached_clock(true);
    s.set_cached_clock(false);
    s.set(b"k", b"v".to_vec(), Some(Duration::from_millis(1)), SetCondition::Always);
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(s.get(b"k"), Ok(None));
}

#[test]
fn a_listing_skips_keys_whose_deadline_passed() {
    let mut s = Store::new();
    s.set(b"live", b"v".to_vec(), None, SetCondition::Always);
    s.set(b"gone", b"v".to_vec(), Some(Duration::from_millis(1)), SetCondition::Always);
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(s.collect_keys(None, None), vec![b"live".to_vec()]);
    let (next, keys) = s.walk_page(0, usize::MAX, None);
    assert_eq!((next, keys), (0, vec![b"live".to_vec()]));
}

#[test]
fn a_paged_walk_visits_every_key_once() {
    let mut s = Store::new();
    for i in 0..100u32 {
        s.set(format!("k{i}").as_bytes(), b"v".to_vec(), None, SetCondition::Always);
    }
    let mut seen = Vec::new();
    let mut cursor = 0;
    loop {
        let (next, keys) = s.walk_page(cursor, 8, None);
        seen.extend(keys);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 100);
}

#[test]
fn a_walk_cursor_naming_an_impossible_capacity_restarts_the_walk() {
    let mut s = Store::new();
    s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    // a capacity of 2^200 cannot exist; the walk starts from bucket 0
    let (next, keys) = s.walk_page((200u64 << 56) | 3, usize::MAX, None);
    assert_eq!((next, keys), (0, vec![b"k".to_vec()]));
}

#[test]
fn a_random_draw_with_repeats_returns_values_only_when_asked() {
    let mut s = Store::new();
    s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
    assert_eq!(s.hrandfield(b"h", -3).unwrap(), vec![b"f".to_vec(); 3]);
    assert_eq!(
        s.hrandfield_with_values(b"h", -2).unwrap(),
        vec![(b"f".to_vec(), b"v".to_vec()); 2]
    );
}

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
#[test]
fn tier_settings_on_an_untiered_store_change_nothing() {
    let mut s = Store::new();
    s.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    let before = s.used_memory();
    s.set_tier_overhead(1 << 20);
    s.tier_reserve_growth();
    assert_eq!(s.used_memory(), before);
    assert_eq!(s.tier_stats().cold_keys, 0);
}
