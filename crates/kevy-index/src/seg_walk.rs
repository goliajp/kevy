//! Walking a [`Segment`] in order without allocating per entry: the
//! crate's [`Walker`] and the public, lending [`Scan`] built on it.

use crate::seg_codec::{Codec, Form, nth_column};
use crate::seg_leaf::Probe;
use crate::seg_tree::{Pos, Tree};
use crate::segment::Segment;
use crate::value::IndexValue;

/// A position moving through a segment's entries, decoding each one's
/// key and value only when asked.
#[derive(Debug)]
pub(crate) struct Walker<'s> {
    tree: &'s Tree,
    codec: &'s Codec,
    next: Option<Pos>,
    rev: bool,
    /// Walking forward, where to stop: at the first entry not below the
    /// bytes, or (`true`) past every entry they prefix.
    until: Option<(Vec<u8>, bool)>,
    e: Vec<u8>,
    payload: &'s [u8],
    vlen: usize,
    value: IndexValue,
    value_ok: bool,
    key: Vec<u8>,
    key_ok: bool,
}

impl<'s> Walker<'s> {
    pub(crate) fn new(seg: &'s Segment, from: Option<Pos>, rev: bool) -> Walker<'s> {
        Walker {
            tree: &seg.tree,
            codec: &seg.codec,
            next: from,
            rev,
            until: None,
            e: Vec::new(),
            payload: &[],
            vlen: 0,
            value: IndexValue::I64(0),
            value_ok: false,
            key: Vec::new(),
            key_ok: false,
        }
    }

    /// Stop at the first entry not below `v`, or with `inclusive` after
    /// the last entry `v` prefixes.
    pub(crate) fn until(mut self, v: Vec<u8>, inclusive: bool) -> Walker<'s> {
        self.until = Some((v, inclusive));
        self
    }

    /// Move to the next entry; `false` when there is none.
    pub(crate) fn advance(&mut self) -> bool {
        let Some(pos) = self.next else { return false };
        if let Some((u, inclusive)) = &self.until {
            let l = self.tree.leaf(pos.leaf);
            let stop = if *inclusive {
                l.cmp_at(&Probe::past(u), pos.slot, &self.tree.ov) == std::cmp::Ordering::Less
            } else {
                l.cmp_at(&Probe::new(u), pos.slot, &self.tree.ov) != std::cmp::Ordering::Greater
            };
            if stop {
                self.next = None;
                return false;
            }
        }
        self.payload = self.tree.entry(pos, &mut self.e);
        self.vlen = match self.codec.form {
            Form::I64 | Form::F64 => 8,
            _ => self.codec.value_len(&self.e),
        };
        self.value_ok = false;
        self.key_ok = false;
        self.next = if self.rev { self.tree.prev_pos(pos) } else { self.tree.next_pos(pos) };
        true
    }

    pub(crate) fn value(&mut self) -> &IndexValue {
        if !self.value_ok {
            self.codec.value_into(&self.e[..self.vlen], &mut self.value);
            self.value_ok = true;
        }
        &self.value
    }

    pub(crate) fn key(&mut self) -> &[u8] {
        if !self.key_ok {
            self.codec.key_into(&self.e[self.vlen..], &mut self.key);
            self.key_ok = true;
        }
        &self.key
    }

    /// Both at once, for callers that want the pair borrowed together.
    pub(crate) fn pair(&mut self) -> (&IndexValue, &[u8]) {
        self.value();
        self.key();
        (&self.value, &self.key)
    }

    pub(crate) fn payload(&self) -> &'s [u8] {
        self.payload
    }

    /// Stored column `field` of the current entry, decoded into `buf`.
    pub(crate) fn column<'b>(&self, field: usize, buf: &'b mut Vec<u8>) -> Option<&'b [u8]>
    where
        's: 'b,
    {
        if field >= self.codec.arity {
            return None;
        }
        nth_column(self.payload, field).bytes(buf)
    }

    /// Every stored column of the current entry.
    pub(crate) fn row(&self) -> Vec<Option<Vec<u8>>> {
        let mut at = 0;
        (0..self.codec.arity)
            .map(|_| crate::seg_codec::column(self.payload, &mut at).to_vec())
            .collect()
    }
}

/// An ordered walk over a [`Segment`], lending each entry.
///
/// The segment stores entries encoded, so a walk decodes each one into
/// buffers it owns and lends them until the next call — no allocation per
/// entry. That is why this is not an `Iterator`.
///
/// ```
/// use kevy_index::{IndexValue, Segment, SortOrder};
/// let mut s = Segment::with_values(1);
/// s.apply_with_values(b"b", None, Some(IndexValue::I64(2)), &[Some(b"x")]);
/// s.apply_with_values(b"a", None, Some(IndexValue::I64(1)), &[None]);
/// let mut scan = s.scan(None, SortOrder::Desc);
/// let mut seen = Vec::new();
/// while let Some((v, k)) = scan.next_entry() {
///     seen.push((v.clone(), k.to_vec(), scan.stored_row()));
/// }
/// assert_eq!(seen[0], (IndexValue::I64(2), b"b".to_vec(), vec![Some(b"x".to_vec())]));
/// assert_eq!(seen[1], (IndexValue::I64(1), b"a".to_vec(), vec![None]));
/// ```
#[derive(Debug)]
pub struct Scan<'s> {
    w: Walker<'s>,
}

impl<'s> Scan<'s> {
    pub(crate) fn new(w: Walker<'s>) -> Scan<'s> {
        Scan { w }
    }

    /// The next entry as `(value, key)`, or `None` at the end.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment, SortOrder};
    /// let mut s = Segment::new();
    /// s.apply(b"k", None, Some(IndexValue::I64(5)));
    /// let mut scan = s.scan(None, SortOrder::Asc);
    /// assert_eq!(scan.next_entry(), Some((&IndexValue::I64(5), &b"k"[..])));
    /// assert_eq!(scan.next_entry(), None);
    /// ```
    pub fn next_entry(&mut self) -> Option<(&IndexValue, &[u8])> {
        if !self.w.advance() {
            return None;
        }
        Some(self.w.pair())
    }

    /// The stored `VALUES` of the entry [`Scan::next_entry`] last
    /// returned, in declared order (empty when the index declares none).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment, SortOrder};
    /// let mut s = Segment::with_values(2);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(5)), &[Some(b"eu"), None]);
    /// let mut scan = s.scan(None, SortOrder::Asc);
    /// scan.next_entry();
    /// assert_eq!(scan.stored_row(), [Some(b"eu".to_vec()), None]);
    /// ```
    pub fn stored_row(&self) -> Vec<Option<Vec<u8>>> {
        self.w.row()
    }
}
