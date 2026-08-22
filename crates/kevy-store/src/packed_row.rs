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

/// The largest total value payload a packed row can address.
pub const PACKED_MAX: usize = u16::MAX as usize;

/// The column names of one declared table, shared by every row in it.
///
/// A packed row has to be able to name its columns — `HGETALL`, the AOF
/// rewrite and the snapshot writer all need field names, and none of them
/// can reach the table catalog, which lives above the store. Carrying the
/// names per row would reintroduce exactly the cost this type removes, so
/// they live here: one allocation per TABLE, cloned into each row as a
/// pointer.
pub type ColumnNames = std::sync::Arc<[Vec<u8>]>;

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
        Some(PackedRow(Box::new(PackedInner {
            cols: names.clone(),
            buf: buf.into_boxed_slice(),
        })))
    }

    /// Declared column count.
    pub fn columns(&self) -> usize {
        u16::from_le_bytes([self.0.buf[0], self.0.buf[1]]) as usize
    }

    /// Whether column `i` is present. Out of range reads as absent.
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
    pub fn get_named(&self, field: &[u8]) -> Option<&[u8]> {
        let i = self.0.cols.iter().position(|c| c == field)?;
        self.get(i)
    }

    /// Whether the row has a column named `field`.
    pub fn has_named(&self, field: &[u8]) -> bool {
        self.0.cols.iter().position(|c| c == field).is_some_and(|i| self.has(i))
    }

    /// Column `i`'s bytes, or `None` when it is absent or out of range.
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

    /// Replace column `i`, rebuilding the row. `None` back when the result
    /// would exceed [`PACKED_MAX`].
    pub fn with_column(&self, i: usize, v: Option<&[u8]>) -> Option<Self> {
        let mut cols: Vec<Option<&[u8]>> = (0..self.columns()).map(|j| self.get(j)).collect();
        *cols.get_mut(i)? = v;
        PackedRow::build(&self.0.cols, &cols)
    }

    /// The column names this row's table declared.
    pub fn names(&self) -> &ColumnNames {
        &self.0.cols
    }

    /// Field name and value for every present column, in declared order —
    /// what `HGETALL`, the rewrite and the snapshot writer need.
    pub fn fields(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        (0..self.columns()).filter_map(move |i| {
            Some((self.0.cols.get(i)?.as_slice(), self.get(i)?))
        })
    }

    /// Total heap bytes of THIS row — the shared column names are one
    /// allocation per table and are not charged per row.
    pub fn heap_bytes(&self) -> usize {
        self.0.buf.len() + core::mem::size_of::<PackedInner>()
    }

    /// The number of present columns, for `HLEN`.
    pub fn len(&self) -> usize {
        (0..self.columns()).filter(|&i| self.has(i)).count()
    }

    /// Whether no column is present.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `n` throwaway column names — the tests are about the payload layout,
    /// not about what the columns are called.
    pub(crate) fn names(n: usize) -> ColumnNames {
        (0..n).map(|i| format!("c{i}").into_bytes()).collect()
    }

    #[test]
    fn round_trips_every_column_including_absent_and_empty() {
        let cols: Vec<Option<&[u8]>> =
            vec![Some(&b"id7"[..]), None, Some(&b""[..]), Some(&b"a longer value"[..])];
        let r = PackedRow::build(&names(cols.len()), &cols).expect("fits");
        assert_eq!(r.columns(), 4);
        for (i, want) in cols.iter().enumerate() {
            assert_eq!(r.get(i), *want, "column {i}");
        }
        // Absent and present-but-empty are different, which is what the
        // bitmap buys over an offset-equality convention.
        assert!(!r.has(1));
        assert!(r.has(2));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn the_cost_scales_with_the_row_rather_than_sitting_on_a_floor() {
        // The defect being removed: a fixed cost independent of shape.
        let three = PackedRow::build(&names(3), &[Some(&b"x"[..]); 3]).expect("fits");
        let twelve = PackedRow::build(&names(12), &[Some(&b"x"[..]); 12]).expect("fits");
        assert!(
            twelve.heap_bytes() > three.heap_bytes(),
            "a wider row must cost more, not the same: {} vs {}",
            three.heap_bytes(),
            twelve.heap_bytes()
        );
        // Payload buffer plus the boxed inner, and nothing else. The inner is
        // the price of `Value`'s 32-byte cap; it is a constant, so it does not
        // reintroduce the floor — it shifts the line the row scales from.
        let inner = core::mem::size_of::<PackedInner>();
        assert_eq!(three.heap_bytes(), (2 + 1 + 3 * 2 + 3) + inner);
        assert_eq!(twelve.heap_bytes(), (2 + 2 + 12 * 2 + 12) + inner);
    }

    #[test]
    fn replacing_a_column_leaves_the_others_alone() {
        let r = PackedRow::build(&names(3), &[Some(&b"a"[..]), Some(&b"bb"[..]), Some(&b"ccc"[..])])
            .expect("fits");
        let r2 = r.with_column(1, Some(b"REPLACED")).expect("fits");
        assert_eq!(r2.get(0), Some(&b"a"[..]));
        assert_eq!(r2.get(1), Some(&b"REPLACED"[..]));
        assert_eq!(r2.get(2), Some(&b"ccc"[..]));
        let r3 = r.with_column(0, None).expect("fits");
        assert!(!r3.has(0));
        assert_eq!(r3.get(2), Some(&b"ccc"[..]));
    }

    #[test]
    fn looks_a_column_up_by_the_name_the_wire_uses() {
        let n: ColumnNames = vec![b"id".to_vec(), b"name".to_vec(), b"dept".to_vec()].into();
        let r = PackedRow::build(&n, &[Some(&b"7"[..]), None, Some(&b"eng"[..])]).expect("fits");
        assert_eq!(r.get_named(b"id"), Some(&b"7"[..]));
        assert_eq!(r.get_named(b"dept"), Some(&b"eng"[..]));
        // Declared but absent on this row, and undeclared, both read as None
        // — but only the first is a column of the table.
        assert_eq!(r.get_named(b"name"), None);
        assert!(!r.has_named(b"name"));
        assert_eq!(r.get_named(b"nosuch"), None);
        assert!(!r.has_named(b"nosuch"));
    }

    #[test]
    fn refuses_a_payload_it_cannot_address() {
        let big = vec![0u8; PACKED_MAX + 1];
        assert!(PackedRow::build(&names(1), &[Some(&big[..])]).is_none());
        let just = vec![0u8; PACKED_MAX];
        assert!(PackedRow::build(&names(1), &[Some(&just[..])]).is_some());
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;
    use super::tests::names;

    /// The claim this type exists for, as arithmetic rather than prose.
    ///
    /// Today a promoted hash costs a 16-slot table (16 × 48 slot bytes plus
    /// 16 + 16 metadata = 800 B requested), an `ArcInner` plus the map struct
    /// (72 B), and a separate chunk for any value past the inline threshold.
    /// A packed row is one buffer.
    #[test]
    fn a_packed_row_costs_less_than_the_table_it_replaces() {
        const TABLE_REQUEST: usize = 16 * 48 + 16 + 16; // slots + metadata
        const ARC_AND_MAP: usize = 16 + 56;
        for (ncol, vlen) in [(3usize, 400usize), (7, 400), (12, 400)] {
            let v = vec![b'x'; vlen / ncol];
            let cols: Vec<Option<&[u8]>> = (0..ncol).map(|_| Some(&v[..])).collect();
            let packed = PackedRow::build(&names(ncol), &cols).expect("fits").heap_bytes();
            let today = TABLE_REQUEST + ARC_AND_MAP + vlen;
            assert!(
                packed * 2 < today,
                "{ncol} columns: packed {packed} B is not less than half of today's {today} B"
            );
        }
    }

    /// And — the point of the finding — the cost must MOVE with the shape.
    #[test]
    fn the_cost_is_not_flat_in_the_column_count() {
        let v = [b'x'; 32];
        let w = |n: usize| {
            PackedRow::build(&names(n), &(0..n).map(|_| Some(&v[..])).collect::<Vec<_>>())
                .expect("fits")
                .heap_bytes()
        };
        let (a, b) = (w(3), w(12));
        // Nine more columns of 32 bytes each, plus nine more ends.
        assert_eq!(b - a, 9 * (32 + 2) + 1, "growth is payload + ends + bitmap byte");
    }
}

#[cfg(test)]
mod read_parity_tests {
    use super::*;
    use crate::{Store, Value};

    /// Two stores holding the same row, one packed and one general.
    fn both() -> (Store, Store, [&'static [u8]; 3]) {
        let cols: [&[u8]; 3] = [b"id", b"name", b"dept"];
        let vals: [Option<&[u8]>; 3] = [Some(b"7"), None, Some(b"eng")];
        let n: ColumnNames = cols.iter().map(|c| c.to_vec()).collect();
        let mut packed = Store::new();
        packed.load_value(b"row:1", &Value::PackedRow(PackedRow::build(&n, &vals).unwrap()), None);
        let mut general = Store::new();
        for (c, v) in cols.iter().zip(vals.iter()) {
            if let Some(v) = v {
                general.hset(b"row:1", &[(c, v)]).unwrap();
            }
        }
        (packed, general, cols)
    }

    /// The per-field verbs must agree for every column, including one the
    /// table declares that this row does not have.
    ///
    /// Every read path ends in a `_ => WrongType` catch-all, so a
    /// representation its arms do not name is not a compile error — it is a
    /// WRONGTYPE at runtime, or a silently empty answer. The compiler cannot
    /// hold this; these tests do.
    #[test]
    fn the_per_field_verbs_agree_with_the_general_hash() {
        let (mut p, mut g, cols) = both();
        assert_eq!(p.hlen(b"row:1").unwrap(), g.hlen(b"row:1").unwrap());
        for c in &cols {
            let name = String::from_utf8_lossy(c);
            assert_eq!(
                p.hget(b"row:1", c).unwrap().map(<[u8]>::to_vec),
                g.hget(b"row:1", c).unwrap().map(<[u8]>::to_vec),
                "HGET {name}"
            );
            assert_eq!(
                p.hexists(b"row:1", c).unwrap(),
                g.hexists(b"row:1", c).unwrap(),
                "HEXISTS {name}"
            );
        }
        assert_eq!(p.hmget(b"row:1", &cols).unwrap(), g.hmget(b"row:1", &cols).unwrap());
    }

    /// The whole-row verbs must agree as sets — the general hash promises no
    /// order, and HGETALL is a flat field/value stream, so it is paired up
    /// before sorting or a field could compare against another field's value.
    #[test]
    fn the_whole_row_verbs_agree_with_the_general_hash() {
        let (mut p, mut g, _) = both();
        let sorted = |mut v: Vec<Vec<u8>>| {
            v.sort();
            v
        };
        assert_eq!(sorted(p.hkeys(b"row:1").unwrap()), sorted(g.hkeys(b"row:1").unwrap()));
        assert_eq!(sorted(p.hvals(b"row:1").unwrap()), sorted(g.hvals(b"row:1").unwrap()));
        let paired = |v: Vec<Vec<u8>>| {
            let mut q: Vec<(Vec<u8>, Vec<u8>)> =
                v.chunks(2).map(|c| (c[0].clone(), c[1].clone())).collect();
            q.sort();
            q
        };
        assert_eq!(
            paired(p.hgetall(b"row:1").unwrap()),
            paired(g.hgetall(b"row:1").unwrap())
        );
    }
}
