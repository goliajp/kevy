use super::*;
use crate::set::KevySet;
use std::cell::Cell;

#[test]
fn new_is_empty() {
    let m: KevyMap<u64, u64> = KevyMap::new();
    assert_eq!(m.len(), 0);
    assert!(m.is_empty());
    assert_eq!(m.capacity(), 0);
    assert_eq!(m.get(&5u64), None);
    assert!(!m.contains_key(&5u64));
}

#[test]
fn insert_get() {
    let mut m = KevyMap::<u64, u64>::new();
    assert!(m.insert(1, 10).is_none());
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&1u64), Some(&10));
    assert_eq!(m.get(&2u64), None);
    assert!(m.contains_key(&1u64));
}

#[test]
fn insert_duplicate_replaces_value_returns_old() {
    let mut m = KevyMap::<u64, u64>::new();
    assert_eq!(m.insert(1, 10), None);
    assert_eq!(m.insert(1, 20), Some(10));
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&1u64), Some(&20));
}

#[test]
fn remove_returns_value_decreases_len() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 10);
    m.insert(2, 20);
    assert_eq!(m.remove(&1u64), Some(10));
    assert_eq!(m.len(), 1);
    assert_eq!(m.remove(&1u64), None);
    assert_eq!(m.get(&1u64), None);
    assert_eq!(m.get(&2u64), Some(&20));
}

#[test]
fn tombstone_reused_on_reinsert() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 10);
    m.remove(&1u64);
    assert_eq!(m.len(), 0);
    m.insert(1, 30);
    assert_eq!(m.len(), 1);
    assert_eq!(m.get(&1u64), Some(&30));
}

#[test]
fn grow_preserves_all_entries_10k() {
    let mut m = KevyMap::<u64, u64>::new();
    for i in 0..10_000u64 {
        m.insert(i, i.wrapping_mul(7));
    }
    assert_eq!(m.len(), 10_000);
    for i in 0..10_000u64 {
        assert_eq!(m.get(&i), Some(&i.wrapping_mul(7)));
    }
}

#[test]
fn byte_string_keys_with_borrow_lookup() {
    let mut m = KevyMap::<Vec<u8>, u64>::new();
    m.insert(b"foo".to_vec(), 1);
    m.insert(b"bar".to_vec(), 2);
    assert_eq!(m.get(b"foo".as_slice()), Some(&1));
    assert_eq!(m.get(b"missing".as_slice()), None);
    assert_eq!(m.remove(b"bar".as_slice()), Some(2));
    assert_eq!(m.len(), 1);
    assert!(!m.contains_key(b"bar".as_slice()));
}

#[test]
fn iter_yields_all_entries() {
    let mut m = KevyMap::<u64, u64>::new();
    for i in 0..20u64 {
        m.insert(i, i + 100);
    }
    let mut seen: Vec<(u64, u64)> = m.iter().map(|(&k, &v)| (k, v)).collect();
    seen.sort();
    let expected: Vec<(u64, u64)> = (0..20).map(|i| (i, i + 100)).collect();
    assert_eq!(seen, expected);
}

struct DropCount<'a>(&'a Cell<usize>);
impl Drop for DropCount<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn clear_drops_entries_and_resets_len() {
    let counter = Cell::new(0);
    let mut m: KevyMap<u64, DropCount<'_>> = KevyMap::new();
    for i in 0..50u64 {
        m.insert(i, DropCount(&counter));
    }
    assert_eq!(m.len(), 50);
    m.clear();
    assert_eq!(m.len(), 0);
    assert_eq!(counter.get(), 50);
    assert!(m.capacity() >= 50);
    m.insert(0, DropCount(&counter));
    assert_eq!(m.len(), 1);
    drop(m);
    assert_eq!(counter.get(), 51);
}

#[test]
fn drop_runs_for_remaining_entries() {
    let counter = Cell::new(0);
    {
        let mut m: KevyMap<u64, DropCount<'_>> = KevyMap::new();
        for i in 0..30u64 {
            m.insert(i, DropCount(&counter));
        }
        m.remove(&5u64);
        assert_eq!(counter.get(), 1);
    }
    assert_eq!(counter.get(), 30);
}

#[test]
fn grow_then_remove_then_grow_again_stays_consistent() {
    let mut m = KevyMap::<u64, u64>::new();
    for i in 0..2000u64 {
        m.insert(i, i);
    }
    for i in 0..1000u64 {
        assert_eq!(m.remove(&i), Some(i));
    }
    for i in 2000..4000u64 {
        m.insert(i, i);
    }
    assert_eq!(m.len(), 3000);
    for i in 1000..4000u64 {
        assert_eq!(m.get(&i), Some(&i));
    }
    for i in 0..1000u64 {
        assert_eq!(m.get(&i), None);
    }
}

#[test]
fn with_capacity_preallocates() {
    let m: KevyMap<u64, u64> = KevyMap::with_capacity(100);
    // ceil(100 * 8 / 7) = 115 → next_pow2 = 128
    assert_eq!(m.capacity(), 128);
    let m: KevyMap<u64, u64> = KevyMap::with_capacity(0);
    assert_eq!(m.capacity(), 0);
    let m: KevyMap<u64, u64> = KevyMap::with_capacity(1);
    assert_eq!(m.capacity(), MIN_CAP);
}

#[test]
fn get_mut_allows_mutation() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 10);
    *m.get_mut(&1u64).unwrap() = 20;
    assert_eq!(m.get(&1u64), Some(&20));
    assert!(m.get_mut(&2u64).is_none());
}

#[test]
fn debug_format_matches_map_shape() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 10);
    m.insert(2, 20);
    let s = format!("{m:?}");
    // Order is unspecified but both entries must appear and the shape is a map.
    assert!(s.starts_with('{'));
    assert!(s.ends_with('}'));
    assert!(s.contains("1: 10") || s.contains("1:10"));
    assert!(s.contains("2: 20") || s.contains("2:20"));
}

#[test]
fn into_iter_ref_works() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 10);
    m.insert(2, 20);
    let mut total = 0u64;
    for (k, v) in &m {
        total += *k + *v;
    }
    assert_eq!(total, 1 + 10 + 2 + 20);
}

#[test]
fn many_collisions_via_long_byte_keys() {
    // Stresses the linear probing loop on a real keyspace shape (variable-
    // length byte keys; the hasher avalanches via fmix64 so h2 distribution
    // is uniform — exercises real-world probe chains rather than a
    // degenerate collision storm).
    let mut m = KevyMap::<Vec<u8>, u64>::new();
    let n = 5_000u64;
    for i in 0..n {
        let k = format!("session:{i:08}:user").into_bytes();
        m.insert(k, i);
    }
    assert_eq!(m.len(), n as usize);
    for i in 0..n {
        let k = format!("session:{i:08}:user");
        assert_eq!(m.get(k.as_bytes()), Some(&i));
    }
}

#[test]
fn zst_value_type() {
    let mut m = KevyMap::<u64, ()>::new();
    assert!(m.insert(1, ()).is_none());
    assert!(m.insert(1, ()).is_some());
    assert!(m.contains_key(&1u64));
    assert_eq!(m.remove(&1u64), Some(()));
}

#[test]
fn set_basic_ops() {
    let mut s: KevySet<Vec<u8>> = KevySet::new();
    assert!(s.insert(b"a".to_vec()));
    assert!(!s.insert(b"a".to_vec())); // duplicate ⇒ false
    assert_eq!(s.len(), 1);
    assert!(s.contains(b"a".as_slice()));
    assert!(!s.contains(b"b".as_slice()));
    assert!(s.remove(b"a".as_slice()));
    assert!(!s.remove(b"a".as_slice()));
    assert!(s.is_empty());
}

#[test]
fn set_iter_yields_members() {
    let mut s: KevySet<u64> = KevySet::new();
    for i in 0..10u64 {
        s.insert(i);
    }
    let mut got: Vec<u64> = s.iter().copied().collect();
    got.sort();
    assert_eq!(got, (0..10u64).collect::<Vec<_>>());
}

#[test]
fn prefetch_for_hash_is_safe_on_any_state() {
    // Just exercise the API on empty and populated tables; it's a hint
    // with no observable side effect, so we can only test it doesn't
    // panic / miscompile.
    let m: KevyMap<u64, u64> = KevyMap::new();
    m.prefetch_for_hash(0);
    m.prefetch_for_hash(u64::MAX);
    let mut m = KevyMap::<u64, u64>::new();
    for i in 0..50u64 {
        m.insert(i, i);
    }
    for i in 0..50u64 {
        m.prefetch_for_hash(i.kevy_hash());
    }
}

#[test]
fn capacity_grows_doubling_from_min_cap() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 1);
    assert_eq!(m.capacity(), MIN_CAP);
    // Fill to just past 7/8 of MIN_CAP: threshold = 14
    for i in 2..=14u64 {
        m.insert(i, i);
    }
    // 14 entries, threshold = 14, next insert grows.
    m.insert(15, 15);
    assert_eq!(m.capacity(), MIN_CAP * 2);
}

// ---- API-surface smoke tests (coverage padding for delegating methods) --

#[test]
fn map_keys_iter() {
    let mut m = KevyMap::<u64, u64>::new();
    for i in 0..5u64 {
        m.insert(i, i + 10);
    }
    let mut ks: Vec<u64> = m.keys().copied().collect();
    ks.sort();
    assert_eq!(ks, vec![0, 1, 2, 3, 4]);
}

#[test]
fn map_values_iter() {
    let mut m = KevyMap::<u64, u64>::new();
    for i in 0..5u64 {
        m.insert(i, i + 10);
    }
    let mut vs: Vec<u64> = m.values().copied().collect();
    vs.sort();
    assert_eq!(vs, vec![10, 11, 12, 13, 14]);
}

#[test]
fn map_iter_mut_writes_visible_via_get() {
    let mut m = KevyMap::<u64, u64>::new();
    assert!(m.iter_mut().next().is_none()); // cap == 0 path

    for i in 0..50u64 {
        m.insert(i, i);
    }
    for i in (0..50u64).step_by(2) {
        m.remove(&i); // tombstones must be skipped, not yielded
    }
    let mut seen = 0usize;
    for (&k, v) in m.iter_mut() {
        assert_eq!(k % 2, 1);
        *v += 100;
        seen += 1;
    }
    assert_eq!(seen, 25);
    for i in (1..50u64).step_by(2) {
        assert_eq!(m.get(&i), Some(&(i + 100)));
    }
}

#[test]
fn map_default_is_empty() {
    let m: KevyMap<u64, u64> = KevyMap::default();
    assert!(m.is_empty());
    assert_eq!(m.capacity(), 0);
}

#[test]
fn map_from_iterator() {
    let m: KevyMap<u64, u64> = (0..10u64).map(|i| (i, i * 2)).collect();
    assert_eq!(m.len(), 10);
    assert_eq!(m.get(&5u64), Some(&10));
}

#[test]
fn map_extend() {
    let mut m = KevyMap::<u64, u64>::new();
    m.extend((0..5u64).map(|i| (i, i)));
    assert_eq!(m.len(), 5);
    assert_eq!(m.get(&3u64), Some(&3));
}

#[test]
fn map_index_panics_on_missing() {
    let mut m = KevyMap::<u64, u64>::new();
    m.insert(1, 10);
    assert_eq!(m[&1u64], 10);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = m[&99u64];
    }));
    assert!(r.is_err(), "Index on missing key should panic");
}

#[test]
fn set_with_capacity_capacity_clear() {
    let mut s: KevySet<u64> = KevySet::with_capacity(50);
    assert!(s.capacity() >= 50);
    for i in 0..10u64 {
        s.insert(i);
    }
    assert_eq!(s.len(), 10);
    s.clear();
    assert!(s.is_empty());
    // capacity preserved
    assert!(s.capacity() >= 50);
}

#[test]
fn set_as_map_smoke() {
    let mut s: KevySet<u64> = KevySet::new();
    s.insert(7);
    assert_eq!(s.as_map().len(), 1);
    assert!(s.as_map().contains_key(&7u64));
}

#[test]
fn set_default_debug() {
    let s: KevySet<u64> = KevySet::default();
    assert!(s.is_empty());
    let dbg = format!("{s:?}");
    assert_eq!(dbg, "{}");
}

#[test]
fn set_into_iter_ref() {
    let mut s: KevySet<u64> = KevySet::new();
    for i in 0..3u64 {
        s.insert(i);
    }
    let mut sum = 0u64;
    for k in &s {
        sum += k;
    }
    assert_eq!(sum, 3);
}

#[test]
fn set_from_iterator() {
    let s: KevySet<u64> = (0..5u64).collect();
    assert_eq!(s.len(), 5);
    assert!(s.contains(&3u64));
}

#[test]
fn set_extend() {
    let mut s: KevySet<u64> = KevySet::new();
    s.extend(0..5u64);
    assert_eq!(s.len(), 5);
}
