//! [`Segment`]'s reads: ranges, point lookups, counts, ordered walks, and
//! the window cut.

use std::cmp::Ordering;

use super::Segment;
use crate::seg_codec::rank_of;
use crate::seg_leaf::Probe;
use crate::seg_tree::Pos;
use crate::seg_walk::{Scan, Walker};
use crate::segment::Cursor;
use crate::value::IndexValue;
use kevy_text::SortOrder;

/// Where a value falls against a segment's own type.
enum Edge {
    /// Before every entry (a lower type, or an empty segment).
    Low,
    /// After every entry (a higher type).
    High,
    /// Its encoding.
    At(Vec<u8>),
}

impl Segment {
    fn edge(&self, v: &IndexValue) -> Edge {
        match rank_of(v).cmp(&self.codec.form.rank()) {
            _ if self.tree.len == 0 => Edge::Low,
            Ordering::Less => Edge::Low,
            Ordering::Greater => Edge::High,
            Ordering::Equal => {
                let mut b = Vec::with_capacity(16);
                self.codec.put_value(v, &mut b);
                Edge::At(b)
            }
        }
    }

    /// The first entry holding a value at or above `v`.
    fn from_value(&self, v: &IndexValue) -> Option<Pos> {
        match self.edge(v) {
            Edge::Low => self.tree.first_pos(),
            Edge::High => None,
            Edge::At(b) => self.tree.lower_bound(&Probe::new(&b)),
        }
    }

    /// The first entry after `(v, key)`.
    fn after(&self, v: &IndexValue, key: &[u8]) -> Option<Pos> {
        match self.edge(v) {
            Edge::Low => self.tree.first_pos(),
            Edge::High => None,
            Edge::At(mut b) if self.codec.fits_key(key) => {
                self.codec.put_handle(key, &mut b);
                let p = Probe::new(&b);
                let pos = self.tree.lower_bound(&p)?;
                let l = self.tree.leaf(pos.leaf);
                match l.cmp_at(&p, pos.slot, &self.tree.ov) {
                    Ordering::Equal => self.tree.next_pos(pos),
                    _ => Some(pos),
                }
            }
            Edge::At(b) => self.skip_keys(&b, key, false),
        }
    }

    /// The last entry before `(v, key)`.
    fn before(&self, v: &IndexValue, key: &[u8]) -> Option<Pos> {
        match self.edge(v) {
            Edge::Low => None,
            Edge::High => self.tree.last_pos(),
            Edge::At(mut b) if self.codec.fits_key(key) => {
                self.codec.put_handle(key, &mut b);
                self.tree.before(&Probe::new(&b))
            }
            Edge::At(b) => self.skip_keys(&b, key, true),
        }
    }

    /// For a bound whose key the segment cannot encode: walk value `vb`'s
    /// run comparing decoded keys, and give the first entry after `key`
    /// (or, `back`, the last one before it).
    fn skip_keys(&self, vb: &[u8], key: &[u8], back: bool) -> Option<Pos> {
        let mut pos = self.tree.lower_bound(&Probe::new(vb));
        let mut e = Vec::new();
        let mut k = Vec::new();
        while let Some(p) = pos {
            self.tree.entry(p, &mut e);
            if !e.starts_with(vb) {
                break;
            }
            self.codec.key_into(&e[vb.len()..], &mut k);
            if (back && k.as_slice() >= key) || (!back && k.as_slice() > key) {
                break;
            }
            pos = self.tree.next_pos(p);
        }
        match (back, pos) {
            (false, p) => p,
            (true, Some(p)) => self.tree.prev_pos(p),
            (true, None) => self.tree.last_pos(),
        }
    }

    /// A forward walk from `start` through every entry whose value is at
    /// most `max`.
    fn walk_to(&self, start: Option<Pos>, max: &IndexValue) -> Walker<'_> {
        let w = Walker::new(self, start, false);
        match self.edge(max) {
            Edge::Low => Walker::new(self, None, false),
            Edge::High => w,
            Edge::At(b) => w.until(b, true),
        }
    }

    /// The `[min, max]` walk resuming after `cursor`, for the clause
    /// engine.
    pub(crate) fn range_walk(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
    ) -> Walker<'_> {
        let start = match cursor {
            Some(c) => self.after(&c.value, &c.key),
            None => self.from_value(min),
        };
        self.walk_to(start, max)
    }

    /// Ordered scan of `[min, max]` (inclusive), resuming after `cursor`,
    /// up to `limit` hits. Returns `(key, value)` pairs in `(value, key)`
    /// order plus the cursor to resume from (`None` = exhausted).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"a", 1), (b"b", 2), (b"c", 3)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// let (page, next) = s.range(&IndexValue::I64(1), &IndexValue::I64(3), None, 2);
    /// assert_eq!(page.len(), 2);
    /// let (rest, _) = s.range(&IndexValue::I64(1), &IndexValue::I64(3), next.as_ref(), 2);
    /// assert_eq!(rest, vec![(b"c".to_vec(), IndexValue::I64(3))]);
    /// ```
    pub fn range(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> (Vec<(Vec<u8>, IndexValue)>, Option<Cursor>) {
        let mut w = self.range_walk(min, max, cursor);
        let mut out = Vec::with_capacity(limit.min(64));
        while out.len() < limit && w.advance() {
            let (v, k) = w.pair();
            out.push((k.to_vec(), v.clone()));
        }
        let next = if out.len() == limit {
            out.last().map(|(k, v)| Cursor { value: v.clone(), key: k.clone() })
        } else {
            None
        };
        (out, next)
    }

    /// Point lookup: every key holding exactly `value` (unique kind's
    /// read; more than one hit is the fence's `-DUPLICATE` signal).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for k in [b"b", b"a"] { s.apply(k, None, Some(IndexValue::I64(1))); }
    /// assert_eq!(s.eq(&IndexValue::I64(1), 10), vec![b"a".to_vec(), b"b".to_vec()]);
    /// ```
    pub fn eq(&self, value: &IndexValue, limit: usize) -> Vec<Vec<u8>> {
        let mut w = self.range_walk(value, value, None);
        let mut out = Vec::new();
        while out.len() < limit && w.advance() {
            out.push(w.key().to_vec());
        }
        out
    }

    /// Count within `[min, max]`, from the counts the tree keeps — no
    /// entry is visited.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for i in 0..100 { s.apply(format!("k{i}").as_bytes(), None, Some(IndexValue::I64(i))); }
    /// assert_eq!(s.count(&IndexValue::I64(10), &IndexValue::I64(19)), 10);
    /// ```
    pub fn count(&self, min: &IndexValue, max: &IndexValue) -> u64 {
        let lo = match self.edge(min) {
            Edge::Low => 0,
            Edge::High => self.tree.len,
            Edge::At(b) => self.tree.rank(&Probe::new(&b)),
        };
        let hi = match self.edge(max) {
            Edge::Low => 0,
            Edge::High => self.tree.len,
            Edge::At(b) => self.tree.rank(&Probe::past(&b)),
        };
        hi.saturating_sub(lo) as u64
    }

    /// The largest value present, if any — the window boundary's read.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"a", 4), (b"b", 9)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// assert_eq!(s.max_value(), Some(IndexValue::I64(9)));
    /// ```
    pub fn max_value(&self) -> Option<IndexValue> {
        let mut w = Walker::new(self, self.tree.last_pos(), true);
        w.advance().then(|| w.value().clone())
    }

    /// Entries strictly below `bound`, in order — the read-only preview
    /// of [`Segment::split_off_below`]'s batch.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"a", 1), (b"b", 5)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// let mut below = s.scan_below(&IndexValue::I64(5));
    /// assert_eq!(below.next_entry(), Some((&IndexValue::I64(1), &b"a"[..])));
    /// assert_eq!(below.next_entry(), None, "the bound itself stays out");
    /// ```
    pub fn scan_below(&self, bound: &IndexValue) -> Scan<'_> {
        let w = Walker::new(self, self.tree.first_pos(), false);
        Scan::new(match self.edge(bound) {
            Edge::Low => Walker::new(self, None, false),
            Edge::High => w,
            Edge::At(b) => w.until(b, false),
        })
    }

    /// Detach every entry whose value sorts below `bound`, in order — the
    /// window-eviction cut. The segment's books (entries, duplicates,
    /// stored values, key directory) end as if each entry had been
    /// removed one by one; entries AT `bound` stay.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"a", 1), (b"b", 1), (b"c", 5)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// let cut = s.split_off_below(&IndexValue::I64(5));
    /// assert_eq!(cut.len(), 2);
    /// assert_eq!((s.stats().entries, s.stats().duplicates), (1, 0));
    /// ```
    pub fn split_off_below(&mut self, bound: &IndexValue) -> Vec<(IndexValue, Vec<u8>)> {
        let raw = match self.edge(bound) {
            Edge::Low => return Vec::new(),
            Edge::High => self.tree.cut_below(&Probe::past(&[])),
            Edge::At(b) => self.tree.cut_below(&Probe::new(&b)),
        };
        let mut out = Vec::with_capacity(raw.len());
        let mut dups = 0u64;
        let mut run = (0usize, Vec::new());
        for (e, _) in &raw {
            let vlen = self.codec.value_len(e);
            if run.1.as_slice() == &e[..vlen] {
                run.0 += 1;
                if run.0 == 2 {
                    dups += 1;
                }
            } else {
                run = (1, e[..vlen].to_vec());
            }
            let mut key = Vec::new();
            self.codec.key_into(&e[vlen..], &mut key);
            out.push((self.codec.value(&e[..vlen]), key));
        }
        self.note_cut(dups, &out);
        out
    }

    /// Ordered streaming scan over the whole segment, ascending or
    /// descending, resuming exclusively past `after`.
    ///
    /// ```
    /// use kevy_index::{Cursor, IndexValue, Segment, SortOrder};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"a", 1), (b"b", 2), (b"c", 3)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// let past = Cursor::new(IndexValue::I64(2), b"b".to_vec());
    /// let mut scan = s.scan(Some(&past), SortOrder::Desc);
    /// assert_eq!(scan.next_entry(), Some((&IndexValue::I64(1), &b"a"[..])));
    /// assert_eq!(scan.next_entry(), None);
    /// ```
    pub fn scan(&self, after: Option<&Cursor>, order: SortOrder) -> Scan<'_> {
        let w = match order {
            SortOrder::Asc => {
                let start =
                    after.map_or_else(|| self.tree.first_pos(), |c| self.after(&c.value, &c.key));
                Walker::new(self, start, false)
            }
            SortOrder::Desc => {
                let start =
                    after.map_or_else(|| self.tree.last_pos(), |c| self.before(&c.value, &c.key));
                Walker::new(self, start, true)
            }
        };
        Scan::new(w)
    }

    /// Visit every `(key, value)` entry, in order (verify / audit walks).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for (k, v) in [(b"b", 1), (b"a", 2)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// let mut seen = Vec::new();
    /// s.each_entry(|k, v| seen.push((k.to_vec(), v.clone())));
    /// assert_eq!(seen, [(b"b".to_vec(), IndexValue::I64(1)), (b"a".to_vec(), IndexValue::I64(2))]);
    /// ```
    pub fn each_entry<F: FnMut(&[u8], &IndexValue)>(&self, mut f: F) {
        let mut w = Walker::new(self, self.tree.first_pos(), false);
        while w.advance() {
            let (v, k) = w.pair();
            f(k, v);
        }
    }
}
