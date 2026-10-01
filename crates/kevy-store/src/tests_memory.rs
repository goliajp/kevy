//! `used_memory` accounting + eviction tests. Split from `tests.rs`
//! to keep both under the 500-LOC house rule.

use super::*;
use crate::tests::s;

// ───────────── used_memory + eviction (Wave 2 task #1) ─────────────

/// The bytes of a keyspace table that has held `keys` keys at once.
fn table_for(keys: usize) -> u64 {
    let mut st = Store::new();
    for i in 0..keys {
        st.set(format!("t{i}").as_bytes(), s("v"), None, crate::SetCondition::Always);
    }
    st.map.footprint() as u64
}

#[test]
fn used_memory_grows_on_insert_shrinks_on_delete() {
    let mut st = Store::new();
    assert_eq!(st.used_memory(), 0);
    // values too long to sit inline, so each key adds heap of its own
    st.set(b"k", s(&"hello".repeat(8)), None, crate::SetCondition::Always);
    let after_one = st.used_memory();
    assert!(after_one > 0, "set should bump used_memory");
    st.set(b"k2", s(&"world".repeat(8)), None, crate::SetCondition::Always);
    assert!(st.used_memory() > after_one, "second set should bump again");
    st.del(&[b"k".as_slice(), b"k2".as_slice()]);
    // the table keeps its slots, and its charge
    assert_eq!(st.used_memory(), st.map.footprint() as u64, "all dels leave the empty table");
}

#[test]
fn used_memory_tracks_collection_growth() {
    // A.8: use field+value sizes that exceed the SmallHashInline budget
    // (22 B packed) so the encoding promotes to the heap-backed Hash
    // path; otherwise the inline encoding (zero heap) makes the
    // accounting deltas zero, which is correct but defeats this test's
    // invariant ("growth should bump used_memory").
    let big_field1: Vec<u8> = vec![b'a'; 30];
    let big_field2: Vec<u8> = vec![b'b'; 30];
    let big_val: Vec<u8> = vec![b'v'; 30];
    let mut st = Store::new();
    st.hset(b"h", &[(big_field1.as_slice(), big_val.as_slice())]).unwrap();
    let after_one_field = st.used_memory();
    st.hset(b"h", &[(big_field2.as_slice(), big_val.as_slice())]).unwrap();
    assert!(st.used_memory() > after_one_field);
    st.hdel(b"h", &[big_field2.as_slice()]).unwrap();
    let after_one_remaining = st.used_memory();
    // shrinking by one field should return us close to the after_one_field
    // baseline (allow slack for hashtable rehash slot accounting).
    let diff = after_one_field.abs_diff(after_one_remaining);
    assert!(diff < 64, "expected close match, got {after_one_field} vs {after_one_remaining}");
}

#[test]
fn used_memory_on_flush_is_the_emptied_table() {
    let mut st = Store::new();
    for i in 0..20 {
        st.set(format!("k{i}").as_bytes(), s("v"), None, crate::SetCondition::Always);
    }
    assert!(st.used_memory() > 0);
    st.flushall();
    assert!(st.map.footprint() > 0, "a flush keeps the table's allocation");
    assert_eq!(st.used_memory(), st.map.footprint() as u64);
}

#[test]
fn precheck_refuses_when_over_limit_with_no_eviction() {
    let mut st = Store::new();
    st.set_max_memory(1, EvictionPolicy::NoEviction);
    st.set(b"k", s("aaaaaaaaaaaaaaaaaaaa"), None, crate::SetCondition::Always);
    assert!(st.used_memory() > 1);
    assert_eq!(st.precheck_for_write(), Err(StoreError::OutOfMemory));
}

#[test]
fn precheck_zero_cost_when_unlimited() {
    let st = Store::new();
    assert_eq!(st.precheck_for_write(), Ok(()));
    // a fresh store with maxmemory=0 must NEVER refuse a write; this is the
    // contract for the embedded / unlimited mode.
}

#[test]
fn allkeys_lru_evicts_least_recent() {
    let mut st = Store::new();
    // the keys' own bytes are held to 1,000 above the table they fill
    st.set_max_memory(table_for(50) + 1_000, EvictionPolicy::AllKeysLru);
    // Fill until we cross the limit; the oldest key should be the victim.
    for i in 0..50 {
        let k = format!("k{i:02}");
        st.set(k.as_bytes(), s(&"x".repeat(60)), None, crate::SetCondition::Always);
        st.try_evict_after_write();
    }
    let limit = table_for(50) + 1_000;
    assert!(st.used_memory() <= limit, "eviction should bring us under: got {}", st.used_memory());
    // Earlier keys should be gone; later keys present.
    assert_eq!(st.get(b"k00"), Ok(None));
    assert_eq!(st.get(b"k49").map(|v| v.is_some()), Ok(true));
}

#[test]
fn allkeys_random_evicts_under_limit() {
    let mut st = Store::new();
    st.set_max_memory(table_for(40) + 600, EvictionPolicy::AllKeysRandom);
    for i in 0..40 {
        let k = format!("k{i:02}");
        st.set(k.as_bytes(), s(&"y".repeat(40)), None, crate::SetCondition::Always);
        st.try_evict_after_write();
    }
    assert!(st.used_memory() <= table_for(40) + 600);
    assert!(st.evictions_total() > 0);
}

#[test]
fn volatile_lru_skips_keys_without_ttl() {
    use std::time::Duration;
    let mut st = Store::new();
    st.set_max_memory(1_500, EvictionPolicy::VolatileLru);
    // permanent keys — should never be evicted
    for i in 0..10 {
        let k = format!("p{i}");
        st.set(k.as_bytes(), s("xxxxxxxxxxxxxxxxxxxx"), None, crate::SetCondition::Always);
    }
    // volatile keys — eligible
    for i in 0..30 {
        let k = format!("v{i}");
        st.set(
            k.as_bytes(),
            s("xxxxxxxxxxxxxxxxxxxx"),
            Some(Duration::from_hours(1)),
            crate::SetCondition::Always,
        );
        st.try_evict_after_write();
    }
    // Permanent keys must all survive.
    for i in 0..10 {
        let k = format!("p{i}");
        assert!(
            st.get(k.as_bytes()).unwrap().is_some(),
            "volatile policy must not evict permanent key {k}"
        );
    }
}

#[test]
fn memory_usage_reports_key_bytes() {
    let mut st = Store::new();
    st.set(b"short", s("v"), None, crate::SetCondition::Always);
    st.set(b"big", s(&"x".repeat(200)), None, crate::SetCondition::Always);
    let small = st.estimate_key_bytes(b"short").unwrap();
    let big = st.estimate_key_bytes(b"big").unwrap();
    // each key carries its half of the table, and nothing but that when
    // its value sits inline
    assert_eq!(small, (st.map.footprint() as u64).div_ceil(2));
    assert!(big > small, "large value should report more bytes: {small} vs {big}");
    assert_eq!(st.estimate_key_bytes(b"missing"), None);
}

#[test]
fn detached_entries_take_every_key_and_leave_none() {
    let mut st = Store::new();
    for i in 0..1000 {
        st.set(format!("k{i}").as_bytes(), vec![b'v'; 64], None, crate::SetCondition::Always);
    }
    st.rpush(b"list", &[b"a".as_slice(), b"b"]).unwrap();
    let detached = st.detach_entries();
    assert_eq!(detached.len(), 1001);
    assert!(st.get(b"k0").unwrap().is_none());
    assert_eq!(st.dbsize(), 0);
    // the store stays usable for what a closing host still does with it
    st.set(b"after", b"x".to_vec(), None, crate::SetCondition::Always);
    assert_eq!(st.get(b"after").unwrap().as_deref(), Some(&b"x"[..]));
    drop(detached);
}
