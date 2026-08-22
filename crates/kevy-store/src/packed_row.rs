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

/// A declared row's values, in declared column order, in one allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedRow(Box<[u8]>);

impl PackedRow {
    /// Build from one value per declared column, `None` for an absent one.
    ///
    /// `None` back when the payload exceeds [`PACKED_MAX`] or the column
    /// count exceeds `u16` — the caller keeps the general form.
    pub fn build(cols: &[Option<&[u8]>]) -> Option<Self> {
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
        Some(PackedRow(buf.into_boxed_slice()))
    }

    /// Declared column count.
    pub fn columns(&self) -> usize {
        u16::from_le_bytes([self.0[0], self.0[1]]) as usize
    }

    /// Whether column `i` is present. Out of range reads as absent.
    pub fn has(&self, i: usize) -> bool {
        i < self.columns() && self.0[2 + i / 8] & (1 << (i % 8)) != 0
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
            u16::from_le_bytes([self.0[at], self.0[at + 1]]) as usize
        };
        let start = if i == 0 { 0 } else { end_at(i - 1) };
        Some(&self.0[header + start..header + end_at(i)])
    }

    /// Replace column `i`, rebuilding the row. `None` back when the result
    /// would exceed [`PACKED_MAX`].
    pub fn with_column(&self, i: usize, v: Option<&[u8]>) -> Option<Self> {
        let mut cols: Vec<Option<&[u8]>> = (0..self.columns()).map(|j| self.get(j)).collect();
        *cols.get_mut(i)? = v;
        PackedRow::build(&cols)
    }

    /// Total heap bytes — the whole row is one allocation.
    pub fn heap_bytes(&self) -> usize {
        self.0.len()
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
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_column_including_absent_and_empty() {
        let cols: Vec<Option<&[u8]>> =
            vec![Some(&b"id7"[..]), None, Some(&b""[..]), Some(&b"a longer value"[..])];
        let r = PackedRow::build(&cols).expect("fits");
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
    fn one_allocation_scales_with_the_row_not_with_a_floor() {
        // The defect being removed: a fixed cost independent of shape.
        let three = PackedRow::build(&[Some(&b"x"[..]); 3]).expect("fits");
        let twelve = PackedRow::build(&[Some(&b"x"[..]); 12]).expect("fits");
        assert!(
            twelve.heap_bytes() > three.heap_bytes(),
            "a wider row must cost more, not the same: {} vs {}",
            three.heap_bytes(),
            twelve.heap_bytes()
        );
        // And the whole row is one allocation: header + payload, nothing else.
        assert_eq!(three.heap_bytes(), 2 + 1 + 3 * 2 + 3);
    }

    #[test]
    fn replacing_a_column_leaves_the_others_alone() {
        let r = PackedRow::build(&[Some(&b"a"[..]), Some(&b"bb"[..]), Some(&b"ccc"[..])])
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
    fn refuses_a_payload_it_cannot_address() {
        let big = vec![0u8; PACKED_MAX + 1];
        assert!(PackedRow::build(&[Some(&big[..])]).is_none());
        let just = vec![0u8; PACKED_MAX];
        assert!(PackedRow::build(&[Some(&just[..])]).is_some());
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;

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
            let packed = PackedRow::build(&cols).expect("fits").heap_bytes();
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
            PackedRow::build(&(0..n).map(|_| Some(&v[..])).collect::<Vec<_>>())
                .expect("fits")
                .heap_bytes()
        };
        let (a, b) = (w(3), w(12));
        // Nine more columns of 32 bytes each, plus nine more ends.
        assert_eq!(b - a, 9 * (32 + 2) + 1, "growth is payload + ends + bitmap byte");
    }
}
