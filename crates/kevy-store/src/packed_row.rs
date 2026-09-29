//! `PackedRow` — a declared table's row as one allocation.
//!
//! A hash on a declared prefix has a known column order, so the row does not
//! need a table to answer "which field is `dept`" and does not need to carry
//! the field names at all. What it needs is the values and where each one
//! starts.
//!
//! The measured defect this removes is not that the general representation
//! is large but that it is **flat**: a hash of three fields and a hash of
//! twelve cost the same 1,700 bytes of RSS, because `KevyMap` rounds to
//! `MIN_CAP = 16` and a promoted hash asks for `with_capacity(1)`, so every
//! hash from one to fourteen fields allocates the same 16-slot table. Here
//! every term scales with the row's actual shape instead.
//!
//! ```text
//! [ncol u16][present bitmap ⌈ncol/8⌉][end_1 u16] … [end_n u16][values …]
//! ```
//!
//! Ends rather than starts: column `i` occupies `end[i-1] .. end[i]`, with
//! `end[-1]` the first byte after the header, so a length is one subtraction
//! and no separate length array exists. A column that is absent has its bit
//! clear; a column that is present and empty has the bit set and a zero-width
//! span — the two are distinct, which `HEXISTS` needs and an offset-equality
//! convention could not express.
//!
//! `u16` ends cap a packed row at 64 KiB of values. Callers build through
//! [`PackedRow::build`], which returns `None` past that, and the caller keeps
//! the general representation — a size class, not a failure.
//!
//! ```
//! use kevy_store::packed_row::{ColumnNames, PackedRow};
//! let names: ColumnNames = vec![b"id".to_vec(), b"dept".to_vec(), b"note".to_vec()].into();
//! let row = PackedRow::build(&names, &[Some(&b"7"[..]), Some(&b""[..]), None]).expect("fits");
//! // present-and-empty and absent stay distinct
//! assert_eq!(row.get_named(b"dept"), Some(&b""[..]));
//! assert_eq!(row.get_named(b"note"), None);
//! assert_eq!(row.len(), 2);
//! ```

/// The largest total value payload a packed row can address.
///
/// ```
/// use kevy_store::packed_row::{ColumnNames, PACKED_MAX, PackedRow};
/// let names: ColumnNames = vec![b"blob".to_vec()].into();
/// let fits = vec![0u8; PACKED_MAX];
/// let too_big = vec![0u8; PACKED_MAX + 1];
/// assert!(PackedRow::build(&names, &[Some(&fits[..])]).is_some());
/// assert!(PackedRow::build(&names, &[Some(&too_big[..])]).is_none());
/// ```
pub const PACKED_MAX: usize = u16::MAX as usize;

/// The column names of one declared table, shared by every row in it.
///
/// A packed row has to be able to name its columns — `HGETALL`, the AOF
/// rewrite and the snapshot writer all need field names, and none of them
/// can reach the table catalog, which lives above the store. Carrying the
/// names per row would reintroduce exactly the cost this type removes, so
/// they live here: one allocation per TABLE, cloned into each row as a
/// pointer.
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
#[cfg(not(feature = "std"))]
use alloc::vec;

/// The column names a table's packed rows share, held once behind an
/// `Arc` rather than per row — the whole point of the packed form is that
/// a million rows of one table carry one copy of the names between them.
///
/// ```
/// use kevy_store::packed_row::{ColumnNames, PackedRow};
/// let names: ColumnNames = vec![b"id".to_vec(), b"dept".to_vec()].into();
/// let a = PackedRow::build(&names, &[Some(&b"1"[..]), Some(&b"eng"[..])]).unwrap();
/// let b = PackedRow::build(&names, &[Some(&b"2"[..]), None]).unwrap();
/// // both rows point at the one shared list of names
/// assert!(std::sync::Arc::ptr_eq(a.names(), b.names()));
/// ```
pub type ColumnNames = alloc::sync::Arc<[Vec<u8>]>;

/// A declared row's values, in declared column order, plus a shared pointer
/// to its table's column names.
///
/// Boxed as one indirection because `Value` is capped at 32 bytes and
/// `Entry` at 48 — assertions that exist so a new variant cannot quietly
/// undo the box-collection win, and they caught this one. The row is
/// therefore two allocations, not one: a 48 B inner and the payload buffer.
/// Costed against the alternatives before choosing — carrying the names
/// behind an `Arc` in `Value` is 560 B for the measured row, this is 544 B,
/// and a bare table id with no names at all would be 496 B but leaves the
/// rewrite and the snapshot writer unable to name a column, which is the
/// problem being solved.
///
/// ```
/// use kevy_store::packed_row::{ColumnNames, PackedRow};
/// let names: ColumnNames = vec![b"id".to_vec(), b"name".to_vec(), b"dept".to_vec()].into();
/// let row = PackedRow::build(&names, &[Some(&b"7"[..]), None, Some(&b"eng"[..])]).unwrap();
/// assert_eq!(row.get_named(b"dept"), Some(&b"eng"[..]));
/// assert_eq!(row.len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedRow(Box<PackedInner>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackedInner {
    cols: ColumnNames,
    buf: Box<[u8]>,
}

impl PackedRow {
    /// Build from one value per declared column, `None` for an absent one.
    ///
    /// `None` back when the payload exceeds [`PACKED_MAX`] or the column
    /// count exceeds `u16` — the caller keeps the general form.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"id".to_vec(), b"note".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"7"[..]), Some(&b""[..])]).unwrap();
    /// assert_eq!(row.get(0), Some(&b"7"[..]));
    /// assert_eq!(row.get(1), Some(&b""[..])); // present and empty, not absent
    /// ```
    pub fn build(names: &ColumnNames, cols: &[Option<&[u8]>]) -> Option<Self> {
        debug_assert_eq!(names.len(), cols.len(), "one value slot per declared column");
        let ncol = u16::try_from(cols.len()).ok()?;
        let total: usize = cols.iter().flatten().map(|v| v.len()).sum();
        if total > PACKED_MAX {
            return None;
        }
        let bitmap = ncol.div_ceil(8) as usize;
        let header = 2 + bitmap + cols.len() * 2;
        let mut buf = vec![0u8; header + total];
        buf[..2].copy_from_slice(&ncol.to_le_bytes());
        let mut end = 0usize;
        for (i, c) in cols.iter().enumerate() {
            if let Some(v) = c {
                buf[2 + i / 8] |= 1 << (i % 8);
                buf[header + end..header + end + v.len()].copy_from_slice(v);
                end += v.len();
            }
            let at = 2 + bitmap + i * 2;
            buf[at..at + 2].copy_from_slice(&(end as u16).to_le_bytes());
        }
        Some(PackedRow(Box::new(PackedInner { cols: names.clone(), buf: buf.into_boxed_slice() })))
    }

    /// Declared column count.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"a".to_vec(), b"b".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"1"[..]), None]).unwrap();
    /// assert_eq!(row.columns(), 2); // declared, whether present or not
    /// ```
    pub fn columns(&self) -> usize {
        u16::from_le_bytes([self.0.buf[0], self.0.buf[1]]) as usize
    }

    /// Whether column `i` is present. Out of range reads as absent.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"a".to_vec(), b"b".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"1"[..]), None]).unwrap();
    /// assert!(row.has(0));
    /// assert!(!row.has(1));
    /// assert!(!row.has(5));
    /// ```
    pub fn has(&self, i: usize) -> bool {
        i < self.columns() && self.0.buf[2 + i / 8] & (1 << (i % 8)) != 0
    }

    /// The value of the column named `field`, or `None` when the table has
    /// no such column or this row does not have it.
    ///
    /// Linear over the column names, which is the right shape here: a
    /// declared table has a handful of columns, and a scan of that many
    /// short slices beats a per-row hash table — the per-row hash table
    /// being the thing this type exists to delete.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"id".to_vec(), b"dept".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"7"[..]), Some(&b"eng"[..])]).unwrap();
    /// assert_eq!(row.get_named(b"dept"), Some(&b"eng"[..]));
    /// assert_eq!(row.get_named(b"nosuch"), None);
    /// ```
    pub fn get_named(&self, field: &[u8]) -> Option<&[u8]> {
        let i = self.0.cols.iter().position(|c| c == field)?;
        self.get(i)
    }

    /// Whether the row has a column named `field`.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"id".to_vec(), b"dept".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"7"[..]), None]).unwrap();
    /// assert!(row.has_named(b"id"));
    /// assert!(!row.has_named(b"dept"));
    /// ```
    pub fn has_named(&self, field: &[u8]) -> bool {
        self.0.cols.iter().position(|c| c == field).is_some_and(|i| self.has(i))
    }

    /// Column `i`'s bytes, or `None` when it is absent or out of range.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"a".to_vec(), b"b".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"x"[..]), Some(&b"yy"[..])]).unwrap();
    /// assert_eq!(row.get(1), Some(&b"yy"[..]));
    /// assert_eq!(row.get(2), None);
    /// ```
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        if !self.has(i) {
            return None;
        }
        let bitmap = (self.columns() as u16).div_ceil(8) as usize;
        let header = 2 + bitmap + self.columns() * 2;
        let end_at = |j: usize| {
            let at = 2 + bitmap + j * 2;
            u16::from_le_bytes([self.0.buf[at], self.0.buf[at + 1]]) as usize
        };
        let start = if i == 0 { 0 } else { end_at(i - 1) };
        Some(&self.0.buf[header + start..header + end_at(i)])
    }

    /// Overwrite column `i` in place when the new value is exactly as wide as
    /// the old one, so the offsets do not move.
    ///
    /// `false` back when it does not fit that shape — a different width, an
    /// absent column becoming present, or an index past the end — and the
    /// caller rebuilds through [`PackedRow::with_column`].
    ///
    /// This is an opportunistic fast path, never a property the design
    /// assumes: whether an update keeps its width is a property of the COLUMN
    /// (a unix-ms timestamp always does, an enum usually does not, an i64
    /// counter does until it crosses a digit), and the declaration carries
    /// types but a type does not fix a width.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"ts".to_vec()].into();
    /// let mut row = PackedRow::build(&names, &[Some(&b"1000"[..])]).unwrap();
    /// assert!(row.set_same_width(0, b"2000"));
    /// assert!(!row.set_same_width(0, b"30000")); // wider: rebuild with `with_column`
    /// assert_eq!(row.get(0), Some(&b"2000"[..]));
    /// ```
    pub fn set_same_width(&mut self, i: usize, v: &[u8]) -> bool {
        let Some(old) = self.get(i) else { return false };
        if old.len() != v.len() {
            return false;
        }
        let bitmap = (self.columns() as u16).div_ceil(8) as usize;
        let header = 2 + bitmap + self.columns() * 2;
        let start = if i == 0 {
            0
        } else {
            let at = 2 + bitmap + (i - 1) * 2;
            u16::from_le_bytes([self.0.buf[at], self.0.buf[at + 1]]) as usize
        };
        self.0.buf[header + start..header + start + v.len()].copy_from_slice(v);
        true
    }

    /// Replace column `i`, rebuilding the row. `None` back when the result
    /// would exceed [`PACKED_MAX`].
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"a".to_vec(), b"b".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"1"[..]), Some(&b"2"[..])]).unwrap();
    /// let row = row.with_column(1, Some(&b"longer"[..])).unwrap();
    /// assert_eq!(row.get(1), Some(&b"longer"[..]));
    /// assert!(!row.with_column(0, None).unwrap().has(0));
    /// ```
    pub fn with_column(&self, i: usize, v: Option<&[u8]>) -> Option<Self> {
        let mut cols: Vec<Option<&[u8]>> = (0..self.columns()).map(|j| self.get(j)).collect();
        *cols.get_mut(i)? = v;
        PackedRow::build(&self.0.cols, &cols)
    }

    /// The column names this row's table declared.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"id".to_vec()].into();
    /// let row = PackedRow::build(&names, &[None]).unwrap();
    /// assert_eq!(&row.names()[..], [b"id".to_vec()]);
    /// ```
    pub fn names(&self) -> &ColumnNames {
        &self.0.cols
    }

    /// Field name and value for every present column, in declared order —
    /// what `HGETALL`, the rewrite and the snapshot writer need.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"id".to_vec(), b"x".to_vec(), b"dept".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"7"[..]), None, Some(&b"eng"[..])]).unwrap();
    /// let got: Vec<(&[u8], &[u8])> = row.fields().collect();
    /// assert_eq!(got, [(&b"id"[..], &b"7"[..]), (b"dept", b"eng")]);
    /// ```
    pub fn fields(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        (0..self.columns())
            .filter_map(move |i| Some((self.0.cols.get(i)?.as_slice(), self.get(i)?)))
    }

    /// Total heap bytes of THIS row — the shared column names are one
    /// allocation per table and are not charged per row.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"v".to_vec()].into();
    /// let small = PackedRow::build(&names, &[Some(&b"x"[..])]).unwrap();
    /// let large = PackedRow::build(&names, &[Some(&[b'x'; 100][..])]).unwrap();
    /// assert_eq!(large.heap_bytes() - small.heap_bytes(), 99);
    /// ```
    pub fn heap_bytes(&self) -> usize {
        self.0.buf.len() + core::mem::size_of::<PackedInner>()
    }

    /// The number of present columns, for `HLEN`.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"a".to_vec(), b"b".to_vec()].into();
    /// let row = PackedRow::build(&names, &[Some(&b"1"[..]), None]).unwrap();
    /// assert_eq!(row.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        (0..self.columns()).filter(|&i| self.has(i)).count()
    }

    /// Whether no column is present.
    ///
    /// ```
    /// use kevy_store::packed_row::{ColumnNames, PackedRow};
    /// let names: ColumnNames = vec![b"a".to_vec()].into();
    /// assert!(PackedRow::build(&names, &[None]).unwrap().is_empty());
    /// assert!(!PackedRow::build(&names, &[Some(&b"1"[..])]).unwrap().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests;

impl crate::Store {
    /// Whether `key` currently holds the packed representation.
    ///
    /// For tests and for `MEMORY`-style introspection only. The
    /// representation is deliberately invisible on the wire; a caller that
    /// branched on it would be depending on something it must not, and the
    /// parity tests exist precisely to prove nothing needs to.
    #[doc(hidden)]
    pub fn is_packed(&mut self, key: &[u8]) -> bool {
        matches!(self.live_entry(key).map(|e| &e.value), Some(crate::Value::PackedRow(_)))
    }

    /// Whether a row under a declared prefix may take the packed form.
    ///
    /// ```
    /// use kevy_store::Store;
    /// let mut s = Store::new();
    /// assert!(!s.packed_rows_enabled());
    /// s.set_packed_rows(true);
    /// assert!(s.packed_rows_enabled());
    /// ```
    pub fn packed_rows_enabled(&self) -> bool {
        self.packed_rows
    }

    /// Allow rows under a declared prefix to take the packed representation.
    ///
    /// Off by default, and settable at runtime, so the two representations
    /// can be compared with the SAME binary — one flag apart rather than two
    /// builds apart.
    ///
    /// ```
    /// use kevy_store::Store;
    /// let mut s = Store::new();
    /// s.set_packed_rows(true);
    /// assert!(s.packed_rows_enabled());
    /// s.set_packed_rows(false);
    /// assert!(!s.packed_rows_enabled());
    /// ```
    pub fn set_packed_rows(&mut self, on: bool) {
        self.packed_rows = on;
    }

    /// Whether `key` already holds the packed form — the common case on every
    /// write after the first, so it is checked before anything reads the row.
    fn already_packed(&mut self, key: &[u8]) -> bool {
        self.is_packed(key)
    }

    /// Convert `key`'s hash into the packed form for a table declaring
    /// `names`, if it is a hash that is not packed already.
    ///
    /// A value the row holds under a name the table does not declare would be
    /// lost, so its presence refuses the conversion outright and the row keeps
    /// the general form. Nothing here may drop a value.
    ///
    /// ```
    /// use kevy_store::Store;
    /// let mut s = Store::new();
    /// s.hset(b"user:1", &[(b"id".as_slice(), b"7".as_slice()), (b"dept", b"eng")])?;
    /// s.pack_row(b"user:1", &[b"id".to_vec(), b"name".to_vec(), b"dept".to_vec()]);
    /// // the representation changed; what the key answers did not
    /// assert_eq!(s.hget(b"user:1", b"dept")?, Some(&b"eng"[..]));
    /// assert_eq!(s.hlen(b"user:1")?, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn pack_row(&mut self, key: &[u8], names: &[Vec<u8>]) {
        if self.already_packed(key) {
            return;
        }
        let Ok(Some(pairs)) = self.hash_pairs(key) else { return };
        if pairs.iter().any(|(f, _)| !names.iter().any(|n| n == f)) {
            return;
        }
        let cols: Vec<Option<&[u8]>> = names
            .iter()
            .map(|n| pairs.iter().find(|(f, _)| f == n).map(|(_, v)| v.as_slice()))
            .collect();
        let shared: ColumnNames = names.to_vec().into();
        let Some(row) = PackedRow::build(&shared, &cols) else { return };
        if let Some(e) = self.live_entry_mut(key) {
            e.value = crate::Value::PackedRow(row);
        }
        self.reweigh_entry(key);
    }
}
