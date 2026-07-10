//! Fuzz `kevy_map::KevyMap` operation sequences against a
//! `std::collections::BTreeMap` oracle.
//!
//! KevyMap is the per-shard keyspace hashtable — the widest-radius stone
//! in the workspace (every command path goes through it). Invariants
//! asserted across arbitrary op streams:
//!
//!   * insert / remove / get / get_mut / contains_key return exactly what
//!     the oracle returns, for every prefix of the op stream
//!   * bulk inserts cross capacity growth / rehash boundaries and the
//!     table still agrees with the oracle afterwards
//!   * `iter` yields exactly the oracle's entry set (no dup, no loss);
//!     `keys`/`values` counts agree
//!   * `iter_from_bucket(start)` yields exactly a suffix of `iter()`'s
//!     bucket-order walk (the documented eviction-sampler contract)
//!   * `prefetch_for_hash` is safe for any hash (present or absent key)
//!     and doesn't perturb reads
//!   * `clear` empties the table

#![no_main]

use kevy_map::{KevyHash, KevyMap};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Input<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    /// Short key from the stream: length 0..=8 so collisions,
    /// re-insertions and tombstone reuse are frequent.
    fn key(&mut self) -> Option<Vec<u8>> {
        let len = (self.byte()? % 9) as usize;
        let end = (self.pos + len).min(self.data.len());
        let k = self.data[self.pos..end].to_vec();
        self.pos = end;
        Some(k)
    }
}

fn check_full_equivalence(map: &KevyMap<Vec<u8>, u64>, oracle: &BTreeMap<Vec<u8>, u64>) {
    assert_eq!(map.len(), oracle.len(), "len diverged");
    assert_eq!(map.is_empty(), oracle.is_empty());
    let mut got: Vec<(Vec<u8>, u64)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    got.sort();
    let want: Vec<(Vec<u8>, u64)> = oracle.iter().map(|(k, v)| (k.clone(), *v)).collect();
    assert_eq!(got, want, "iter contents diverged from oracle");
    assert_eq!(map.keys().count(), map.len());
    assert_eq!(map.values().count(), map.len());
}

fuzz_target!(|data: &[u8]| {
    let mut input = Input { data, pos: 0 };
    // First byte seeds the capacity hint so both the lazy cap=0 path and
    // pre-sized tables are exercised.
    let cap_hint = input.byte().unwrap_or(0) as usize;
    let mut map: KevyMap<Vec<u8>, u64> = KevyMap::with_capacity(cap_hint % 64);
    let mut oracle: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let mut counter: u64 = 0;

    while let Some(op) = input.byte() {
        match op % 8 {
            0 | 1 => {
                let Some(k) = input.key() else { break };
                counter += 1;
                assert_eq!(
                    map.insert(k.clone(), counter),
                    oracle.insert(k, counter),
                    "insert return diverged"
                );
            }
            2 => {
                let Some(k) = input.key() else { break };
                assert_eq!(map.remove(k.as_slice()), oracle.remove(&k), "remove diverged");
            }
            3 => {
                let Some(k) = input.key() else { break };
                assert_eq!(map.get(k.as_slice()), oracle.get(&k), "get diverged");
                assert_eq!(
                    map.contains_key(k.as_slice()),
                    oracle.contains_key(&k),
                    "contains_key diverged"
                );
            }
            4 => {
                // prefetch is advisory: must be safe for any hash (incl.
                // hashes of absent keys) and must not perturb reads.
                let Some(k) = input.key() else { break };
                map.prefetch_for_hash(k.as_slice().kevy_hash());
                assert_eq!(map.get(k.as_slice()), oracle.get(&k), "get-after-prefetch diverged");
            }
            5 => {
                let Some(k) = input.key() else { break };
                match (map.get_mut(k.as_slice()), oracle.get_mut(&k)) {
                    (Some(v), Some(w)) => {
                        *v = v.wrapping_add(1);
                        *w = w.wrapping_add(1);
                    }
                    (None, None) => {}
                    (got, want) => panic!("get_mut diverged: {got:?} vs {want:?}"),
                }
            }
            6 => {
                // Bulk insert of fresh keys to force capacity growth and
                // rehash sweeps (up to 1020 entries per op).
                let n = (input.byte().unwrap_or(0) as usize) * 4;
                for _ in 0..n {
                    counter += 1;
                    let k = format!("bulk-{counter}").into_bytes();
                    assert_eq!(map.insert(k.clone(), counter), None);
                    assert_eq!(oracle.insert(k, counter), None);
                }
            }
            7 => {
                check_full_equivalence(&map, &oracle);
                // iter_from_bucket(start) must yield exactly the suffix of
                // the bucket-order walk starting at `start` (both iterators
                // walk buckets ascending).
                let cap = map.capacity();
                if cap > 0 {
                    let start = (input.byte().unwrap_or(0) as usize) % cap;
                    let full: Vec<(Vec<u8>, u64)> =
                        map.iter().map(|(k, v)| (k.clone(), *v)).collect();
                    let tail: Vec<(Vec<u8>, u64)> = map
                        .iter_from_bucket(start)
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    assert!(tail.len() <= full.len());
                    assert_eq!(
                        tail.as_slice(),
                        &full[full.len() - tail.len()..],
                        "iter_from_bucket is not a suffix of iter"
                    );
                }
            }
            _ => unreachable!(),
        }
    }

    check_full_equivalence(&map, &oracle);
    map.clear();
    assert_eq!(map.len(), 0);
    assert!(map.is_empty());
    assert!(map.iter().next().is_none());
});
