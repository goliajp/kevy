//! Unit tests for [`KevyMap::scan_step`] — full-sweep coverage on a
//! fixed table, duplicate-freedom without rehash, and the dictScan
//! guarantee (no key present throughout is skipped) across a grow that
//! happens mid-sweep.

use std::collections::HashMap;

use crate::map::KevyMap;

/// Drive a full sweep (cursor 0 → … → 0), recording how many times each
/// key was visited.
fn full_sweep(m: &KevyMap<u64, u64>) -> HashMap<u64, usize> {
    let mut seen: HashMap<u64, usize> = HashMap::new();
    let mut cursor = 0u64;
    let mut steps = 0usize;
    loop {
        cursor = m.scan_step(cursor, |k, _| {
            *seen.entry(*k).or_insert(0) += 1;
        });
        steps += 1;
        assert!(steps <= m.capacity().max(16), "sweep did not terminate");
        if cursor == 0 {
            break;
        }
    }
    seen
}

#[test]
fn empty_map_is_done_immediately() {
    let m: KevyMap<u64, u64> = KevyMap::new();
    let mut visited = 0;
    assert_eq!(m.scan_step(0, |_, _| visited += 1), 0);
    assert_eq!(visited, 0);
}

#[test]
fn full_coverage_exactly_once_on_fixed_table() {
    // Pre-size so no grow happens during the sweep: every live key must
    // be visited EXACTLY once (home groups partition the keyspace).
    let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(10_000);
    for i in 0..10_000u64 {
        m.insert(i, i * 2);
    }
    let cap_before = m.capacity();
    let seen = full_sweep(&m);
    assert_eq!(m.capacity(), cap_before, "table grew unexpectedly");
    assert_eq!(seen.len(), 10_000, "missed keys");
    for (k, n) in &seen {
        assert_eq!(*n, 1, "key {k} visited {n} times on a static table");
    }
}

#[test]
fn coverage_survives_tombstones() {
    // Deletions leave DELETED metadata; survivors must still all be
    // found exactly once (tombstone-reuse displacement is the tricky
    // path for the run-termination bound).
    let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(4_096);
    for i in 0..4_000u64 {
        m.insert(i, i);
    }
    for i in (0..4_000u64).step_by(3) {
        m.remove(&i);
    }
    // Re-insert a few so tombstone reuse actually happens.
    for i in (0..600u64).step_by(3) {
        m.insert(i, i + 1);
    }
    let want: Vec<u64> = (0..4_000u64).filter(|i| i % 3 != 0 || *i < 600).collect();
    let seen = full_sweep(&m);
    assert_eq!(seen.len(), want.len());
    for k in &want {
        assert_eq!(seen.get(k), Some(&1), "key {k} not visited exactly once");
    }
}

#[test]
fn grow_mid_sweep_never_skips_an_original_key() {
    // The dictScan guarantee: keys present for the WHOLE sweep are
    // returned at least once even when the table rehashes (grows)
    // between steps. Insert 2k keys, scan a few steps, then insert
    // 8k more (forcing at least two capacity doublings), finish the
    // sweep — every original key must have been seen.
    let mut m: KevyMap<u64, u64> = KevyMap::new();
    for i in 0..2_000u64 {
        m.insert(i, i);
    }
    let cap_small = m.capacity();

    let mut seen: HashMap<u64, usize> = HashMap::new();
    let mut cursor = 0u64;
    for _ in 0..8 {
        cursor = m.scan_step(cursor, |k, _| {
            *seen.entry(*k).or_insert(0) += 1;
        });
        assert_ne!(cursor, 0, "sweep finished before the grow could happen");
    }

    for i in 2_000..10_000u64 {
        m.insert(i, i);
    }
    assert!(m.capacity() >= cap_small * 4, "test needs ≥2 doublings");

    let mut steps = 0usize;
    while cursor != 0 {
        cursor = m.scan_step(cursor, |k, _| {
            *seen.entry(*k).or_insert(0) += 1;
        });
        steps += 1;
        assert!(steps <= m.capacity(), "sweep did not terminate");
    }

    for i in 0..2_000u64 {
        assert!(seen.contains_key(&i), "original key {i} skipped across the grow");
    }
}

#[test]
fn cursor_stays_within_group_mask() {
    // Garbage high bits in a client-supplied cursor must be masked away
    // by the increment (dictScan's `v |= ~mask` trick) — the returned
    // cursor always addresses a real group of the current table.
    let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(1_000);
    for i in 0..1_000u64 {
        m.insert(i, i);
    }
    let ngroups = (m.capacity() / 16) as u64;
    let garbage = (0xDEAD_BEEFu64 << 32) | 3;
    let next = m.scan_step(garbage, |_, _| {});
    assert!(next < ngroups, "cursor {next} escaped the group mask");
}
