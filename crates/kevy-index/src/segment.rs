//! [`Segment`] — one shard's slice of one index (index-follows-key).
//!
//! Entries are `(value, key)` pairs kept in value order, ties broken by
//! key, each with the row's stored `VALUES` beside it. They live packed in
//! the leaves of a counted B+ tree, encoded so that byte order is entry
//! order (see the `seg_codec` module). Nothing maps a key back to its
//! entry: a write names the row's old value, which the write path read
//! before it changed the row.

use crate::key_dir::KeyDir;
use crate::seg_codec::{Codec, Form, be8, put_column};
use crate::seg_tree::{Pos, Tree};
use crate::segment_stats::SegmentStats;
use crate::spec::IndexSpec;
use crate::value::IndexValue;

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
    /// for (k, v) in [(b"a", 1), (b"b", 2)] { s.apply(k, None, Some(IndexValue::I64(v))); }
    /// let (_, next) = s.range(&IndexValue::I64(0), &IndexValue::I64(9), None, 1);
    /// assert_eq!(next.expect("more to read").value, IndexValue::I64(1));
    /// ```
    pub value: IndexValue,
    /// Last key served (tiebreak within a value).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for k in [b"x", b"y"] { s.apply(k, None, Some(IndexValue::I64(5))); }
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

/// One shard's slice of one index.
///
/// A write passes the row's value before the write (`old`) and after it
/// (`new`); `None` means the row had, or now has, no entry.
///
/// ```
/// use kevy_index::{IndexValue, Segment};
/// let mut s = Segment::new();
/// s.apply(b"u:1", None, Some(IndexValue::I64(30)));
/// s.apply(b"u:2", None, Some(IndexValue::I64(40)));
/// let (hits, _) = s.range(&IndexValue::I64(35), &IndexValue::I64(50), None, 10);
/// assert_eq!(hits, vec![(b"u:2".to_vec(), IndexValue::I64(40))]);
/// assert_eq!(s.count(&IndexValue::I64(0), &IndexValue::I64(99)), 2);
/// s.remove(b"u:1", &IndexValue::I64(30));
/// assert_eq!(s.eq(&IndexValue::I64(30), 10), Vec::<Vec<u8>>::new());
/// ```
#[derive(Debug)]
pub struct Segment {
    pub(crate) tree: Tree,
    pub(crate) codec: Codec,
    /// `entries` and `approx_bytes` are derived in [`Segment::stats`].
    stats: SegmentStats,
    key_dir: Option<KeyDir>,
    /// Scratch for an order key and a payload, reused by every write.
    ebuf: Vec<u8>,
    pbuf: Vec<u8>,
}

impl Default for Segment {
    fn default() -> Self {
        Segment::with_codec(Codec::new(Form::Unset, b"", 0))
    }
}

impl Segment {
    fn with_codec(codec: Codec) -> Segment {
        Segment {
            tree: Tree::new(codec.arity > 0),
            codec,
            stats: SegmentStats::default(),
            key_dir: None,
            ebuf: Vec::new(),
            pbuf: Vec::new(),
        }
    }

    /// Empty segment. Its value type is set by the first value it holds.
    ///
    /// ```
    /// assert_eq!(kevy_index::Segment::new().stats().entries, 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty segment that stores `n` declared `VALUES` fields per row.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(1)), &[Some(b"eu")]);
    /// assert_eq!(s.stored(&IndexValue::I64(1), b"k", 0), Some(b"eu".to_vec()));
    /// ```
    pub fn with_values(n: usize) -> Self {
        Segment::with_codec(Codec::new(Form::Unset, b"", n))
    }

    /// Empty segment shaped by `spec`: its value type, the key prefix its
    /// rows share (left out of every stored key), a composite value stored
    /// as its own encoding, and the declared `VALUES`. A key outside the
    /// prefix is still accepted; the segment then stores whole keys.
    ///
    /// ```
    /// use kevy_index::{IndexKind, IndexSpec, IndexValue, Segment, ValType};
    /// let spec = IndexSpec::builder("age", "user:", IndexKind::Range, ValType::I64)
    ///     .with_field("age")
    ///     .build()?;
    /// let mut s = Segment::for_spec(&spec);
    /// s.apply(b"user:42", None, Some(IndexValue::I64(30)));
    /// assert_eq!(s.eq(&IndexValue::I64(30), 10), vec![b"user:42".to_vec()]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn for_spec(spec: &IndexSpec) -> Self {
        let form = match (spec.composite(), spec.ty()) {
            (Some(cols), _) => Form::Composite(cols.iter().map(|c| (c.ty, c.order)).collect()),
            (None, crate::ValType::I64) => Form::I64,
            (None, crate::ValType::F64) => Form::F64,
            (None, _) => Form::Str,
        };
        Segment::with_codec(Codec::new(form, spec.prefix(), spec.values().len()))
    }

    /// Write-path maintenance: the row at `key` held `old` and now
    /// coerces to `new` (`None` = excluded / row deleted, counted as a
    /// coerce failure).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"k", None, Some(IndexValue::I64(1)));
    /// s.apply(b"k", Some(&IndexValue::I64(1)), Some(IndexValue::I64(2)));
    /// assert!(s.contains(&IndexValue::I64(2), b"k") && !s.contains(&IndexValue::I64(1), b"k"));
    /// s.apply(b"k", Some(&IndexValue::I64(2)), None);
    /// assert_eq!((s.stats().entries, s.stats().coerce_failures), (0, 1));
    /// ```
    pub fn apply(&mut self, key: &[u8], old: Option<&IndexValue>, new: Option<IndexValue>) {
        self.apply_with_values(key, old, new, &[]);
    }

    /// [`Segment::apply`] plus the row's declared stored values, which
    /// ride with the entry.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(1)), &[Some(b"old")]);
    /// let one = IndexValue::I64(1);
    /// s.apply_with_values(b"k", Some(&one), Some(one.clone()), &[Some(b"new")]);
    /// assert_eq!(s.stored(&one, b"k", 0), Some(b"new".to_vec()), "same value, new stored field");
    /// ```
    pub fn apply_with_values(
        &mut self,
        key: &[u8],
        old: Option<&IndexValue>,
        new: Option<IndexValue>,
        vals: &[Option<&[u8]>],
    ) {
        let Some(v) = new else {
            if let Some(o) = old {
                self.remove(key, o);
            }
            self.stats.coerce_failures += 1;
            return;
        };
        if old.is_some_and(|o| *o == v) && self.codec.arity == 0 {
            return;
        }
        if let Some(o) = old.filter(|o| **o != v) {
            self.remove(key, o);
        }
        if self.codec.form == Form::Unset {
            self.codec.form = Form::of(&v);
        }
        if !self.codec.form.admits(&v) {
            // a value of another type than the segment's: excluded
            self.stats.coerce_failures += 1;
            return;
        }
        self.fit_key(key);
        self.insert(key, &v, vals);
    }

    fn insert(&mut self, key: &[u8], v: &IndexValue, vals: &[Option<&[u8]>]) {
        self.ebuf.clear();
        self.codec.put_entry(v, key, &mut self.ebuf);
        self.pbuf.clear();
        for f in 0..self.codec.arity {
            put_column(vals.get(f).copied().flatten(), &mut self.pbuf);
        }
        let vlen = self.codec.value_len(&self.ebuf);
        let (codec, ebuf) = (&self.codec, &self.ebuf);
        let mut held = 0;
        let new = self.tree.insert_seen(ebuf, &self.pbuf, |t, at| {
            held = holders(t, codec, &ebuf[..vlen], t.prev_pos(at), t.normalize(at));
        });
        if new && held == 1 {
            self.stats.duplicates += 1;
        }
        if let Some(d) = &mut self.key_dir {
            d.put(key, v);
        }
    }

    /// Row deleted: take its entry (held under `old`) out. Not a coerce
    /// failure.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"k", None, Some(IndexValue::I64(1)));
    /// s.remove(b"k", &IndexValue::I64(1));
    /// assert_eq!((s.stats().entries, s.stats().coerce_failures), (0, 0));
    /// ```
    pub fn remove(&mut self, key: &[u8], old: &IndexValue) {
        if !self.codec.form.admits(old) || !self.codec.fits_key(key) {
            return;
        }
        self.ebuf.clear();
        self.codec.put_entry(old, key, &mut self.ebuf);
        let vlen = self.codec.value_len(&self.ebuf);
        let (codec, ebuf) = (&self.codec, &self.ebuf);
        let mut held = 0;
        let gone = self.tree.remove_seen(ebuf, |t, at| {
            held = holders(t, codec, &ebuf[..vlen], t.prev_pos(at), t.next_pos(at));
        });
        if gone && held == 1 {
            self.stats.duplicates -= 1;
        }
        if gone && let Some(d) = &mut self.key_dir {
            d.remove(key);
        }
    }

    /// Re-encode every entry when `key` does not fit the segment's key
    /// form (it lacks the prefix, or is not all digits after it).
    fn fit_key(&mut self, key: &[u8]) {
        if self.codec.fits_key(key) {
            return;
        }
        let wide = self.codec.widened_for(key);
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(self.tree.len);
        let mut w = crate::seg_walk::Walker::new(self, self.tree.first_pos(), false);
        while w.advance() {
            let payload = w.payload().to_vec();
            let (v, k) = w.pair();
            let mut e = Vec::new();
            wide.put_entry(v, k, &mut e);
            entries.push((e, payload));
        }
        self.codec = wide;
        self.tree.rebuild(entries.iter().map(|(e, p)| (e.as_slice(), p.as_slice())));
    }

    /// Whether the segment holds `key` under `value`.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"k", None, Some(IndexValue::I64(7)));
    /// assert!(s.contains(&IndexValue::I64(7), b"k"));
    /// assert!(!s.contains(&IndexValue::I64(8), b"k"));
    /// ```
    pub fn contains(&self, value: &IndexValue, key: &[u8]) -> bool {
        self.find(value, key).is_some()
    }

    /// The position of `(value, key)`, when held.
    pub(crate) fn find(&self, value: &IndexValue, key: &[u8]) -> Option<Pos> {
        if !self.codec.form.admits(value) || !self.codec.fits_key(key) {
            return None;
        }
        let mut e = Vec::with_capacity(24);
        self.codec.put_entry(value, key, &mut e);
        let p = crate::seg_leaf::Probe::new(&e);
        let pos = self.tree.lower_bound(&p)?;
        let l = self.tree.leaf(pos.leaf);
        (l.cmp_at(&p, pos.slot, &self.tree.ov) == std::cmp::Ordering::Equal).then_some(pos)
    }

    /// `key`'s stored value for declared `VALUES` field `field`, where the
    /// row is held under `value`; `None` when the row has none.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::with_values(2);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(1)), &[Some(b"eu"), None]);
    /// assert_eq!(s.stored(&IndexValue::I64(1), b"k", 0), Some(b"eu".to_vec()));
    /// assert_eq!(s.stored(&IndexValue::I64(1), b"k", 1), None);
    /// ```
    pub fn stored(&self, value: &IndexValue, key: &[u8], field: usize) -> Option<Vec<u8>> {
        if field >= self.codec.arity {
            return None;
        }
        let pos = self.find(value, key)?;
        let payload = self.tree.leaf(pos.leaf).tail(pos.slot, &self.tree.ov).payload;
        crate::seg_codec::nth_column(payload, field).to_vec()
    }

    /// Every stored value of the row held under `value`, in declared
    /// `VALUES` order; empty when the index declares none or the row is
    /// not held.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::with_values(2);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(1)), &[Some(b"a"), Some(b"12")]);
    /// assert_eq!(s.stored_row(&IndexValue::I64(1), b"k"), [Some(b"a".to_vec()), Some(b"12".to_vec())]);
    /// ```
    pub fn stored_row(&self, value: &IndexValue, key: &[u8]) -> Vec<Option<Vec<u8>>> {
        let Some(pos) = self.find(value, key) else { return Vec::new() };
        let payload = self.tree.leaf(pos.leaf).tail(pos.slot, &self.tree.ov).payload;
        let mut at = 0;
        (0..self.codec.arity).map(|_| crate::seg_codec::column(payload, &mut at).to_vec()).collect()
    }

    /// Keep a key → value directory beside the entries (`on`), or drop
    /// it. Views ask a row's value by key alone; an index no view reads
    /// has no directory and pays nothing for one.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"k", None, Some(IndexValue::I64(3)));
    /// s.set_key_dir(true);
    /// assert_eq!(s.key_dir().and_then(|d| d.get(b"k")), Some(IndexValue::I64(3)));
    /// ```
    pub fn set_key_dir(&mut self, on: bool) {
        if !on {
            self.key_dir = None;
            return;
        }
        if self.key_dir.is_some() {
            return;
        }
        let mut d = KeyDir::new();
        self.each_entry(|k, v| d.put(k, v));
        self.key_dir = Some(d);
    }

    /// The key → value directory, when [`Segment::set_key_dir`] asked for
    /// one.
    ///
    /// ```
    /// assert!(kevy_index::Segment::new().key_dir().is_none());
    /// ```
    pub fn key_dir(&self) -> Option<&KeyDir> {
        self.key_dir.as_ref()
    }

    /// Live counters. The memory term is every byte the segment's
    /// structures hold: its leaves, its inner nodes and their separators,
    /// out-of-line entries, and the key directory when there is one.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"k", None, Some(IndexValue::I64(3)));
    /// assert_eq!(s.stats().entries, 1);
    /// assert!(s.stats().approx_bytes >= 1784, "at least one leaf");
    /// ```
    pub fn stats(&self) -> SegmentStats {
        let mut s = self.stats;
        s.entries = self.tree.len as u64;
        s.approx_bytes =
            self.tree.heap_bytes() as u64 + self.key_dir.as_ref().map_or(0, KeyDir::approx_bytes);
        s
    }

    /// Repack every entry into full leaves. Entries written in random
    /// order leave leaves about 69% full; a build ends with this, so what
    /// stays resident is the packed form.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for i in 0..10_000 {
    ///     s.apply(format!("k{i}").as_bytes(), None, Some(IndexValue::I64(i * 7919 % 10_000)));
    /// }
    /// let before = s.stats().approx_bytes;
    /// s.repack();
    /// assert!(s.stats().approx_bytes < before);
    /// assert_eq!(s.stats().entries, 10_000);
    /// ```
    pub fn repack(&mut self) {
        self.tree.repack();
    }

    pub(crate) fn note_cut(&mut self, dups: u64, keys: &[(IndexValue, Vec<u8>)]) {
        self.stats.duplicates -= dups;
        if let Some(d) = &mut self.key_dir {
            for (_, k) in keys {
                d.remove(k);
            }
        }
    }
}

/// How many entries next to a spot hold the value `vb`, counting no
/// further than 2: walking left from `left` and right from `right`.
fn holders(t: &Tree, c: &Codec, vb: &[u8], left: Option<Pos>, right: Option<Pos>) -> usize {
    let fixed = matches!(c.form, Form::I64 | Form::F64);
    let head = if fixed { be8(vb) } else { 0 };
    let mut key = Vec::new();
    let mut same = |p: Pos| {
        if fixed {
            return t.leaf(p.leaf).head(p.slot) == head;
        }
        t.entry(p, &mut key);
        key.starts_with(vb)
    };
    let mut n = 0;
    let mut at = left;
    while let Some(p) = at.filter(|_| n < 2) {
        if !same(p) {
            break;
        }
        n += 1;
        at = t.prev_pos(p);
    }
    let mut at = right;
    while let Some(p) = at.filter(|_| n < 2) {
        if !same(p) {
            break;
        }
        n += 1;
        at = t.next_pos(p);
    }
    n
}

#[path = "segment_read.rs"]
mod read;

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
