//! Equality, debug output, iterator size hints and the allocation queries
//! a caller holding memory to a budget reads.

use crate::map::{KevyMap, MIN_CAP, table_layout};
use crate::set::KevySet;

#[test]
fn maps_are_equal_by_contents_not_by_capacity() {
    let a: KevyMap<u64, u8> = [(1, 10), (2, 20)].into_iter().collect();
    let mut b = KevyMap::with_capacity(512);
    b.insert(2, 20);
    b.insert(1, 10);
    assert!(a == b);

    let mut other_value = b.clone();
    other_value.insert(2, 21);
    assert!(a != other_value, "same keys, one value differs");

    let mut other_key = KevyMap::new();
    other_key.insert(1, 10);
    other_key.insert(3, 20);
    assert!(a != other_key, "same length, one key differs");

    b.insert(3, 30);
    assert!(a != b, "lengths differ");
}

#[test]
fn sets_are_equal_by_members() {
    let a: KevySet<u64> = [1, 2, 3].into_iter().collect();
    let b: KevySet<u64> = [3, 2, 1].into_iter().collect();
    let c: KevySet<u64> = [1, 2, 4].into_iter().collect();
    assert!(a == b);
    assert!(a != c);
}

#[test]
fn an_owning_iterator_reports_what_is_left() {
    let m: KevyMap<u64, u64> = (0..5).map(|k| (k, k)).collect();
    let mut it = m.into_iter();
    assert_eq!(format!("{it:?}"), "IntoIter { remaining: 5, .. }");
    it.next();
    it.next();
    assert_eq!(format!("{it:?}"), "IntoIter { remaining: 3, .. }");

    let s: KevySet<u64> = (0..4).collect();
    let mut it = s.into_iter();
    assert_eq!(it.size_hint(), (4, Some(4)));
    it.next();
    assert_eq!(it.size_hint(), (3, Some(3)));
    assert_eq!(it.len(), 3);
    assert_eq!(it.count(), 3);
}

#[test]
fn an_empty_map_has_no_room_and_no_table() {
    let m: KevyMap<u64, u64> = KevyMap::new();
    assert_eq!(m.room(), 0);
    assert!(m.table_allocation().is_none());
}

#[test]
fn a_heap_table_reports_its_block() {
    let mut m: KevyMap<u64, u64> = KevyMap::new();
    m.insert(1, 1);
    let (ptr, layout) = m.table_allocation().expect("a heap table");
    assert!(!ptr.is_null());
    assert_eq!(layout, table_layout::<(u64, u64)>(m.capacity()).0);
}

#[test]
fn a_mapped_table_reports_no_heap_block() {
    // 65536 slots of 17 bytes cross the size above which tables are mapped
    let m: KevyMap<u64, u64> = KevyMap::with_capacity(50_000);
    assert!(m.capacity() >= 65_536);
    assert_eq!(m.table_allocation().is_none(), m.mmap_backed);
}

fn grown_footprint_is_what_the_growth_costs(mut m: KevyMap<u64, u64>) {
    let promised = m.grown_footprint();
    let cap = m.capacity();
    let mut k = m.len() as u64;
    while m.room() > 0 {
        m.insert(k, 0);
        k += 1;
    }
    m.insert(u64::MAX, 0);
    assert_eq!(m.capacity(), if cap == 0 { MIN_CAP } else { cap * 2 });
    assert_eq!(m.footprint(), promised);
}

#[test]
fn grown_footprint_matches_the_table_the_next_growth_builds() {
    grown_footprint_is_what_the_growth_costs(KevyMap::new());
    let mut small = KevyMap::new();
    small.insert(0, 0);
    grown_footprint_is_what_the_growth_costs(small);
    // the grown table is past the size that is mapped directly
    grown_footprint_is_what_the_growth_costs(KevyMap::with_capacity(50_000));
}
