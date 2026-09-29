//! [`Segment`] — one shard's slice of one index (index-follows-key).
//! Range = an ordered set of `(value, key)` rows; Unique = the same
//! tree (point lookups are a 1-value range) plus a duplicate counter
//! for the declarative fence, kept by looking at a value's neighbours
//! in the tree rather than in a per-value table.
//!
//! The runtime keeps a reverse set keyed by row key inside the segment
//! so `apply` can remove a row's OLD entry without re-reading history.
//! Both sides point at one shared row, so a key and its value are held
//! once per index.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::collections::btree_set;
use std::iter::Map;
use std::mem::size_of;
use std::ops::Bound;

use crate::rowvalues::RowValues;
use crate::segment_entry::{ByKey, ByValue, RowRef, row_bytes, share, table_buckets};
use crate::segment_stats::SegmentStats;
use crate::value::IndexValue;
use kevy_text::SortOrder;

/// Opaque pagination cursor: the last `(value, key)` served. Encoded
/// by the runtime into the wire cursor; `None` = start.
///
/// ```
/// use kevy_index::{Cursor, IndexValue};
/// let c = Cursor::new(IndexValue::I64(30), b"user:7".to_vec());
/// assert_eq!(c.key, b"user:7");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cursor {
    /// Last value served.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"a", 1), (b"b", 2)] { s.apply(k, Some(IndexValue::I64(v))); }
    /// let (_, next) = s.range(&IndexValue::I64(0), &IndexValue::I64(9), None, 1);
    /// assert_eq!(next.expect("more to read").value, IndexValue::I64(1));
    /// ```
    pub value: IndexValue,
    /// Last key served (tiebreak within a value).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for k in [b"x", b"y"] { s.apply(k, Some(IndexValue::I64(5))); }
    /// let (_, next) = s.range(&IndexValue::I64(5), &IndexValue::I64(5), None, 1);
    /// assert_eq!(next.expect("more to read").key, b"x", "ties break on the key");
    /// ```
    pub key: Vec<u8>,
}

impl Cursor {
    /// The cursor just past `(value, key)`, the last entry served.
    ///
    /// ```
    /// use kevy_index::{Cursor, IndexValue};
    /// assert_eq!(Cursor::new(IndexValue::I64(1), b"k".to_vec()).value, IndexValue::I64(1));
    /// ```
    pub fn new(value: IndexValue, key: Vec<u8>) -> Cursor {
        Cursor { value, key }
    }
}

// a tree node runs a little over half full, so one 8-byte slot plus its
// share of the node header comes to about two pointer widths per row
const TREE_BYTES_PER_ROW: usize = 2 * size_of::<ByValue>();

/// A `(value, key)` bound built from borrowed parts.
type Probe<'a> = (&'a IndexValue, &'a [u8]);

/// Rows in tree order as `(value, key)`.
pub(crate) type Walk<'s> =
    Map<btree_set::Range<'s, ByValue>, fn(&'s ByValue) -> (&'s IndexValue, &'s [u8])>;

/// One shard's slice of one index.
///
/// ```
/// use kevy_index::{IndexValue, Segment};
/// let mut s = Segment::new();
/// s.apply(b"u:1", Some(IndexValue::I64(30)));
/// s.apply(b"u:2", Some(IndexValue::I64(40)));
/// let (hits, _) = s.range(&IndexValue::I64(35), &IndexValue::I64(50), None, 10);
/// assert_eq!(hits, vec![(b"u:2".to_vec(), IndexValue::I64(40))]);
/// assert_eq!(s.count(&IndexValue::I64(0), &IndexValue::I64(99)), 2);
/// s.remove(b"u:1");
/// assert_eq!(s.eq(&IndexValue::I64(30), 10), Vec::<Vec<u8>>::new());
/// ```
#[derive(Debug, Default)]
pub struct Segment {
    tree: BTreeSet<ByValue>,
    back: HashSet<ByKey>,
    /// The most rows the reverse set has held. Its table never shrinks, so
    /// this sizes its buckets; reading `capacity()` instead depends on
    /// where the seeded hash put the removals' tombstones.
    back_peak: usize,
    /// `approx_bytes` here counts the rows alone; [`Segment::stats`]
    /// adds the two containers' slots.
    stats: SegmentStats,
    /// The stored-value side-channel — `Some` only when the index
    /// declared `VALUES`. An index without the declaration holds `None`
    /// and pays nothing: every values touch below is a never-taken
    /// `if let` (the `Option<Positions>` physical-bypass pattern, A5).
    values: Option<RowValues>,
}

impl Segment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty segment carrying the stored-value side-channel for `n`
    /// declared `VALUES` fields (`n` = the spec's `values.len()`).
    pub fn with_values(n: usize) -> Self {
        Segment { values: Some(RowValues::new(n)), ..Self::default() }
    }

    /// [`Segment::apply`] plus the row's declared stored values. The
    /// values follow the entry: an indexed row stores them, an excluded
    /// or deleted one drops them.
    pub fn apply_with_values(
        &mut self,
        key: &[u8],
        new: Option<IndexValue>,
        vals: &[Option<&[u8]>],
    ) {
        let indexed = new.is_some();
        self.apply(key, new);
        if let Some(rv) = &mut self.values {
            if indexed {
                rv.set(key, vals);
            } else {
                rv.clear(key);
            }
        }
    }

    /// `key`'s stored value for declared `VALUES` field `field`, or
    /// `None` when the row has none (or the index declared none).
    pub fn stored(&self, key: &[u8], field: usize) -> Option<&[u8]> {
        self.values.as_ref()?.get(key, field)
    }

    /// Every stored value of one row, aligned with the declared
    /// `VALUES` order — what an eviction carries into a cold entry's
    /// payload so the clause-carrying cold path never re-reads the
    /// row. Empty when the index declared no values.
    pub fn stored_row(&self, key: &[u8]) -> Vec<Option<&[u8]>> {
        match self.values.as_ref() {
            Some(rv) => (0..rv.arity()).map(|f| rv.get(key, f)).collect(),
            None => Vec::new(),
        }
    }

    /// The tree between two borrowed `(value, key)` bounds, for the
    /// clause-carrying scan. The bounds are only read while seeking, so
    /// the walk borrows the segment alone.
    pub(crate) fn walk<'s>(&'s self, lower: Bound<Probe<'_>>, upper: Bound<Probe<'_>>) -> Walk<'s> {
        let lo = lower.as_ref().map(|p| p as &dyn RowRef);
        let hi = upper.as_ref().map(|p| p as &dyn RowRef);
        self.tree.range::<dyn RowRef, _>((lo, hi)).map(ByValue::pair)
    }

    /// Synchronous write-path maintenance: the row at `key` now
    /// coerces to `new` (`None` = excluded / row deleted). Replaces
    /// any previous entry for the key.
    pub fn apply(&mut self, key: &[u8], new: Option<IndexValue>) {
        let Some(v) = new else {
            self.detach(key);
            self.stats.coerce_failures += 1;
            return;
        };
        if self.back.get(key).is_some_and(|row| *row.value() == v) {
            return;
        }
        self.detach(key);
        self.inc_count(&v);
        self.stats.entries += 1;
        self.stats.approx_bytes += row_bytes(&v, key);
        let (by_value, by_key) = share(v, key);
        self.back.insert(by_key);
        self.back_peak = self.back_peak.max(self.back.len());
        self.tree.insert(by_value);
    }

    /// Take `key`'s row out of both containers and the books.
    fn detach(&mut self, key: &[u8]) {
        let Some(row) = self.back.take(key) else { return };
        self.dec_count(row.value());
        self.tree.remove(&row as &dyn RowRef);
        self.stats.entries -= 1;
        self.stats.approx_bytes -= row_bytes(row.value(), key);
    }

    /// The largest value present, if any — the window boundary's
    /// tree-tail read.
    pub fn max_value(&self) -> Option<&IndexValue> {
        self.tree.last().map(|row| row.value())
    }

    /// Entries strictly below `bound`, tree order — the read-only
    /// preview of [`Self::split_off_below`]'s batch (the slide builds
    /// its segment from this BEFORE cutting, so an I/O failure leaves
    /// the tree untouched).
    pub fn iter_below(&self, bound: &IndexValue) -> impl Iterator<Item = (&IndexValue, &[u8])> {
        self.walk(Bound::Unbounded, Bound::Excluded((bound, &[])))
    }

    /// Detach every entry whose value sorts below `bound`, in tree
    /// order — the window-eviction cut. The detached batch leaves all
    /// of the segment's books (tree, reverse set, duplicate count, stored
    /// values, stats) exactly as if each entry had been removed one by
    /// one; the empty-key sentinel keeps every entry AT `bound` in the
    /// hot tree, so the cut is strictly `< bound`.
    pub fn split_off_below(&mut self, bound: &IndexValue) -> Vec<(IndexValue, Vec<u8>)> {
        let at: Probe<'_> = (bound, &[]);
        let kept = self.tree.split_off(&at as &dyn RowRef);
        let evicted: Vec<ByValue> = core::mem::replace(&mut self.tree, kept).into_iter().collect();
        // the cut is by value, so every holder of an evicted value leaves
        // with it: each run of two or more in the batch was one duplicate
        let dups = evicted.chunk_by(|a, b| a.value() == b.value()).filter(|r| r.len() > 1).count();
        self.stats.duplicates -= dups as u64;
        for row in &evicted {
            self.back.remove(row.key());
            self.stats.entries -= 1;
            self.stats.approx_bytes -= row_bytes(row.value(), row.key());
            if let Some(rv) = &mut self.values {
                rv.clear(row.key());
            }
        }
        evicted.into_iter().map(ByValue::into_parts).collect()
    }

    /// Row deleted (no coercion involved — not a coerce failure).
    pub fn remove(&mut self, key: &[u8]) {
        if let Some(rv) = &mut self.values {
            rv.clear(key);
        }
        self.detach(key);
    }

    /// How many keys hold `v`, counted no further than `cap`.
    fn holders(&self, v: &IndexValue, cap: usize) -> usize {
        self.walk(Bound::Included((v, &[])), Bound::Unbounded)
            .take_while(|(x, _)| *x == v)
            .take(cap)
            .count()
    }

    /// Call before `v` gains a holder in the tree.
    fn inc_count(&mut self, v: &IndexValue) {
        if self.holders(v, 2) == 1 {
            self.stats.duplicates += 1;
        }
    }

    /// Call before `v` loses a holder in the tree.
    fn dec_count(&mut self, v: &IndexValue) {
        if self.holders(v, 3) == 2 {
            self.stats.duplicates -= 1;
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
        let lower = match cursor {
            Some(c) => Bound::Excluded((&c.value, c.key.as_slice())),
            None => Bound::Included((min, &[][..])),
        };
        // Upper bound is exact via take-while — a synthetic sentinel
        // key would MISS max-valued keys sorting above it.
        let mut out = Vec::with_capacity(limit.min(64));
        for (v, k) in self.walk(lower, Bound::Unbounded) {
            if v > max {
                break;
            }
            out.push((k.to_vec(), v.clone()));
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
        self.walk(Bound::Included((value, &[])), Bound::Unbounded)
            .take_while(|(v, _)| *v == value)
            .take(limit)
            .map(|(_, k)| k.to_vec())
            .collect()
    }

    /// Count within `[min, max]` without materializing keys.
    pub fn count(&self, min: &IndexValue, max: &IndexValue) -> u64 {
        self.walk(Bound::Included((min, &[])), Bound::Unbounded)
            .take_while(|(v, _)| *v <= max)
            .count() as u64
    }

    /// Verify hook: what value does the segment hold for `key`?
    /// (`IDX.VERIFY` compares this against a fresh row coercion.)
    pub fn verify_entry(&self, key: &[u8]) -> Option<&IndexValue> {
        self.back.get(key).map(|row| row.value())
    }

    /// Ordered streaming scan over the WHOLE segment: ascending (or
    /// descending) `(value, key)` order, resuming exclusively past
    /// `after`. The virtual-view pager drives this and probes
    /// membership per candidate — O(limit × selectivity⁻¹) instead of
    /// materializing the full member set.
    pub fn scan<'s>(
        &'s self,
        after: Option<&Cursor>,
        order: SortOrder,
    ) -> Box<dyn Iterator<Item = (&'s IndexValue, &'s [u8])> + 's> {
        let past = after.map_or(Bound::Unbounded, |c| Bound::Excluded((&c.value, &c.key[..])));
        match order {
            SortOrder::Asc => Box::new(self.walk(past, Bound::Unbounded)),
            SortOrder::Desc => Box::new(self.walk(Bound::Unbounded, past).rev()),
        }
    }

    /// Visit every `(key, value)` entry (verify / audit walks).
    pub fn each_entry<F: FnMut(&[u8], &IndexValue)>(&self, mut f: F) {
        for row in &self.back {
            f(row.key(), row.value());
        }
    }

    /// Live counters. The memory term is the rows plus both containers'
    /// slots; the stored-value column's heap joins it when (and only
    /// when) the index declared `VALUES`.
    pub fn stats(&self) -> SegmentStats {
        let mut s = self.stats;
        // each bucket is one pointer and one control byte
        let buckets = table_buckets(self.back_peak);
        s.approx_bytes +=
            (self.tree.len() * TREE_BYTES_PER_ROW + buckets * (size_of::<ByKey>() + 1)) as u64;
        if let Some(rv) = &self.values {
            s.approx_bytes += rv.approx_bytes();
        }
        s
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
        let (hits, _) =
            t.range(&IndexValue::Str(b"a".to_vec()), &IndexValue::Str(b"z".to_vec()), None, 10);
        assert_eq!(hits[0].0, b"y".to_vec());
    }
}
