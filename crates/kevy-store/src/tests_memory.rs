//! `used_memory` accounting + eviction tests. Split from `tests.rs`
//! to keep both under the 500-LOC house rule.

use super::*;
use crate::tests::s;

// ───────────── used_memory + eviction (Wave 2 task #1) ─────────────

#[test]
fn used_memory_grows_on_insert_shrinks_on_delete() {
    let mut st = Store::new();
    assert_eq!(st.used_memory(), 0);
    st.set(b"k", s("hello"), None, false, false);
    let after_one = st.used_memory();
    assert!(after_one > 0, "set should bump used_memory");
    st.set(b"k2", s("world"), None, false, false);
    assert!(st.used_memory() > after_one, "second set should bump again");
    st.del(&[s("k"), s("k2")]);
    assert_eq!(st.used_memory(), 0, "all dels should zero used_memory");
}

#[test]
fn used_memory_tracks_collection_growth() {
    let mut st = Store::new();
    st.hset(b"h", &[(s("field1"), s("v"))]).unwrap();
    let after_one_field = st.used_memory();
    st.hset(b"h", &[(s("field2"), s("v"))]).unwrap();
    assert!(st.used_memory() > after_one_field);
    st.hdel(b"h", &[s("field2")]).unwrap();
    let after_one_remaining = st.used_memory();
    // shrinking by one field should return us close to the after_one_field
    // baseline (allow a small slack for slot accounting).
    let diff = after_one_field.abs_diff(after_one_remaining);
    assert!(diff < 16, "expected close match, got {after_one_field} vs {after_one_remaining}");
}

#[test]
fn used_memory_zero_on_flush() {
    let mut st = Store::new();
    for i in 0..20 {
        st.set(format!("k{i}").as_bytes(), s("v"), None, false, false);
    }
    assert!(st.used_memory() > 0);
    st.flush();
    assert_eq!(st.used_memory(), 0);
}

#[test]
fn precheck_refuses_when_over_limit_with_no_eviction() {
    let mut st = Store::new();
    st.set_max_memory(1, EvictionPolicy::NoEviction);
    st.set(b"k", s("aaaaaaaaaaaaaaaaaaaa"), None, false, false);
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
    st.set_max_memory(2_000, EvictionPolicy::AllKeysLru);
    // Fill until we cross the limit; the oldest key should be the victim.
    for i in 0..50 {
        let k = format!("k{i:02}");
        st.set(k.as_bytes(), s("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), None, false, false);
        st.try_evict_after_write();
    }
    assert!(
        st.used_memory() <= 2_000,
        "eviction should bring us under: got {}",
        st.used_memory()
    );
    // Earlier keys should be gone; later keys present.
    assert_eq!(st.get(b"k00"), Ok(None));
    assert_eq!(st.get(b"k49").map(|v| v.is_some()), Ok(true));
}

#[test]
fn allkeys_random_evicts_under_limit() {
    let mut st = Store::new();
    st.set_max_memory(1_500, EvictionPolicy::AllKeysRandom);
    for i in 0..40 {
        let k = format!("k{i:02}");
        st.set(k.as_bytes(), s("yyyyyyyyyyyyyyyyyyyyyy"), None, false, false);
        st.try_evict_after_write();
    }
    assert!(st.used_memory() <= 1_500);
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
        st.set(k.as_bytes(), s("xxxxxxxxxxxxxxxxxxxx"), None, false, false);
    }
    // volatile keys — eligible
    for i in 0..30 {
        let k = format!("v{i}");
        st.set(
            k.as_bytes(),
            s("xxxxxxxxxxxxxxxxxxxx"),
            Some(Duration::from_secs(3600)),
            false,
            false,
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
    st.set(b"short", s("v"), None, false, false);
    let small = st.estimate_key_bytes(b"short").unwrap();
    st.set(b"big", s(&"x".repeat(200)), None, false, false);
    let big = st.estimate_key_bytes(b"big").unwrap();
    assert!(big > small, "large value should report more bytes: {small} vs {big}");
    assert_eq!(st.estimate_key_bytes(b"missing"), None);
}
