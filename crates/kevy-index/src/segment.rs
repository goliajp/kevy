//! [`Segment`] — one shard's slice of one index (index-follows-key,
//! RFC D3). Range = `BTreeMap<(value, key), ()>`; Unique = the same
//! tree (point lookups are a 1-value range) plus a duplicate counter
//! for the declarative fence.
//!
//! The runtime keeps a reverse map `key → value` inside the segment so
//! `apply` can remove a row's OLD entry without re-reading history.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ops::Bound;

use crate::value::IndexValue;

/// Opaque pagination cursor: the last `(value, key)` served. Encoded
/// by the runtime into the wire cursor; `None` = start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Last value served.
    pub value: IndexValue,
    /// Last key served (tiebreak within a value).
    pub key: Vec<u8>,
}

/// Sizing + health counters (`IDX.LIST` / memory formula).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SegmentStats {
    /// Live entries.
    pub entries: u64,
    /// Approximate heap bytes (RFC D7 formula's measured side).
    pub approx_bytes: u64,
    /// Rows excluded because the field failed coercion / was missing.
    pub coerce_failures: u64,
    /// Values currently held by more than one key (unique fence).
    pub duplicates: u64,
}

/// Per-entry structural overhead in the memory formula (RFC D7).
const ENTRY_OVERHEAD: usize = 48;

/// One shard's slice of one index.
#[derive(Debug, Default)]
pub struct Segment {
    tree: BTreeMap<(IndexValue, Vec<u8>), ()>,
    back: HashMap<Vec<u8>, IndexValue>,
    value_counts: BTreeMap<IndexValue, u32>,
    stats: SegmentStats,
}

impl Segment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous write-path maintenance: the row at `key` now
    /// coerces to `new` (`None` = excluded / row deleted). Replaces
    /// any previous entry for the key.
    pub fn apply(&mut self, key: &[u8], new: Option<IndexValue>) {
        if let Some(old) = self.back.remove(key) {
            self.tree.remove(&(old.clone(), key.to_vec()));
            self.stats.entries -= 1;
            self.stats.approx_bytes = self
                .stats
                .approx_bytes
                .saturating_sub((old.approx_bytes() + key.len() + ENTRY_OVERHEAD) as u64);
            self.dec_count(&old);
        }
        match new {
            Some(v) => {
                self.stats.entries += 1;
                self.stats.approx_bytes +=
                    (v.approx_bytes() + key.len() + ENTRY_OVERHEAD) as u64;
                self.inc_count(&v);
                self.back.insert(key.to_vec(), v.clone());
                self.tree.insert((v, key.to_vec()), ());
            }
            None => {
                self.stats.coerce_failures += 1;
            }
        }
    }

    /// Row deleted (no coercion involved — not a coerce failure).
    pub fn remove(&mut self, key: &[u8]) {
        if let Some(old) = self.back.remove(key) {
            self.tree.remove(&(old.clone(), key.to_vec()));
            self.stats.entries -= 1;
            self.stats.approx_bytes = self
                .stats
                .approx_bytes
                .saturating_sub((old.approx_bytes() + key.len() + ENTRY_OVERHEAD) as u64);
            self.dec_count(&old);
        }
    }

    fn inc_count(&mut self, v: &IndexValue) {
        let c = self.value_counts.entry(v.clone()).or_insert(0);
        *c += 1;
        if *c == 2 {
            self.stats.duplicates += 1;
        }
    }

    fn dec_count(&mut self, v: &IndexValue) {
        if let Some(c) = self.value_counts.get_mut(v) {
            if *c == 2 {
                self.stats.duplicates -= 1;
            }
            *c -= 1;
            if *c == 0 {
                self.value_counts.remove(v);
            }
        }
    }

    /// Ordered scan of `[min, max]` (inclusive), resuming after
    /// `cursor`, up to `limit` hits. Returns `(key, value)` pairs in
    /// `(value, key)` order plus the cursor to resume from (`None` =
    /// exhausted).
    pub fn range(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> (Vec<(Vec<u8>, IndexValue)>, Option<Cursor>) {
        let lower: Bound<(IndexValue, Vec<u8>)> = match cursor {
            Some(c) => Bound::Excluded((c.value.clone(), c.key.clone())),
            None => Bound::Included((min.clone(), Vec::new())),
        };
        // Upper bound is exact via take-while — a synthetic sentinel
        // key would MISS max-valued keys sorting above it.
        let mut out = Vec::with_capacity(limit.min(64));
        let mut iter = self.tree.range((lower, Bound::Unbounded));
        for ((v, k), ()) in iter.by_ref() {
            if v > max {
                break;
            }
            out.push((k.clone(), v.clone()));
            if out.len() == limit {
                break;
            }
        }
        let next = if out.len() == limit {
            out.last().map(|(k, v)| Cursor { value: v.clone(), key: k.clone() })
        } else {
            None
        };
        (out, next)
    }

    /// Point lookup: every key holding exactly `value` (unique kind's
    /// read; >1 hit = the declarative fence's `-DUPLICATE` signal).
    pub fn eq(&self, value: &IndexValue, limit: usize) -> Vec<Vec<u8>> {
        let lower = Bound::Included((value.clone(), Vec::new()));
        self.tree
            .range((lower, Bound::Unbounded))
            .take_while(|((v, _), ())| v == value)
            .take(limit)
            .map(|((_, k), ())| k.clone())
            .collect()
    }

    /// Count within `[min, max]` without materializing keys.
    pub fn count(&self, min: &IndexValue, max: &IndexValue) -> u64 {
        let lower = Bound::Included((min.clone(), Vec::new()));
        self.tree
            .range((lower, Bound::Unbounded))
            .take_while(|((v, _), ())| v <= max)
            .count() as u64
    }

    /// Verify hook: what value does the segment hold for `key`?
    /// (`IDX.VERIFY` compares this against a fresh row coercion.)
    pub fn verify_entry(&self, key: &[u8]) -> Option<&IndexValue> {
        self.back.get(key)
    }

    /// Visit every `(key, value)` entry (verify / audit walks).
    pub fn each_entry<F: FnMut(&[u8], &IndexValue)>(&self, mut f: F) {
        for (k, v) in &self.back {
            f(k.as_slice(), v);
        }
    }

    /// Live counters.
    pub fn stats(&self) -> SegmentStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(v: i64) -> IndexValue {
        IndexValue::I64(v)
    }

    fn seeded() -> Segment {
        let mut s = Segment::new();
        for (k, v) in [("u1", 30), ("u2", 25), ("u3", 30), ("u4", 40), ("u5", 18)] {
            s.apply(k.as_bytes(), Some(i(v)));
        }
        s
    }

    #[test]
    fn apply_replace_remove_and_stats() {
        let mut s = seeded();
        assert_eq!(s.stats().entries, 5);
        assert_eq!(s.stats().duplicates, 1, "30 held twice");
        // replace u1's value: 30 no longer duplicated
        s.apply(b"u1", Some(i(31)));
        assert_eq!(s.stats().entries, 5);
        assert_eq!(s.stats().duplicates, 0);
        // coerce-failure excludes and counts
        s.apply(b"u2", None);
        assert_eq!(s.stats().entries, 4);
        assert_eq!(s.stats().coerce_failures, 1);
        // remove is not a coerce failure
        s.remove(b"u3");
        assert_eq!(s.stats().entries, 3);
        assert_eq!(s.stats().coerce_failures, 1);
        assert!(s.verify_entry(b"u3").is_none());
        assert_eq!(s.verify_entry(b"u4"), Some(&i(40)));
    }

    #[test]
    fn range_scan_orders_and_paginates() {
        let s = seeded();
        let (page1, cur) = s.range(&i(18), &i(30), None, 2);
        assert_eq!(page1[0], (b"u5".to_vec(), i(18)));
        assert_eq!(page1[1], (b"u2".to_vec(), i(25)));
        let cur = cur.expect("more pages");
        let (page2, cur2) = s.range(&i(18), &i(30), Some(&cur), 10);
        assert_eq!(
            page2,
            vec![(b"u1".to_vec(), i(30)), (b"u3".to_vec(), i(30))],
            "value tie broken by key"
        );
        assert!(cur2.is_none(), "exhausted");
        assert_eq!(s.count(&i(18), &i(30)), 4);
        assert_eq!(s.count(&i(99), &i(100)), 0);
    }

    #[test]
    fn eq_and_duplicate_fence() {
        let s = seeded();
        assert_eq!(s.eq(&i(30), 10), vec![b"u1".to_vec(), b"u3".to_vec()]);
        assert_eq!(s.eq(&i(40), 10), vec![b"u4".to_vec()]);
        assert!(s.eq(&i(99), 10).is_empty());
    }

    #[test]
    fn long_keys_at_max_value_not_missed() {
        let mut s = Segment::new();
        let long_key = vec![0xFFu8; 80]; // sorts above any 64-byte sentinel
        s.apply(&long_key, Some(i(30)));
        s.apply(b"short", Some(i(30)));
        let (hits, _) = s.range(&i(30), &i(30), None, 10);
        assert_eq!(hits.len(), 2, "max-valued long key must not be missed");
        assert_eq!(s.eq(&i(30), 10).len(), 2);
        assert_eq!(s.count(&i(30), &i(30)), 2);
    }

    #[test]
    fn f64_and_str_orders() {
        let mut s = Segment::new();
        s.apply(b"a", Some(IndexValue::F64(1.5)));
        s.apply(b"b", Some(IndexValue::F64(-0.5)));
        let (hits, _) = s.range(&IndexValue::F64(-1.0), &IndexValue::F64(2.0), None, 10);
        assert_eq!(hits[0].0, b"b".to_vec());

        let mut t = Segment::new();
        t.apply(b"x", Some(IndexValue::Str(b"banana".to_vec())));
        t.apply(b"y", Some(IndexValue::Str(b"apple".to_vec())));
        let (hits, _) = t.range(
            &IndexValue::Str(b"a".to_vec()),
            &IndexValue::Str(b"z".to_vec()),
            None,
            10,
        );
        assert_eq!(hits[0].0, b"y".to_vec());
    }
}
