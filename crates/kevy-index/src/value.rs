//! [`IndexValue`] — the three scalar types an index can hold, with a
//! total order (f64 via `total_cmp`, so NaN coerce-fails upstream and
//! never enters a segment).

use std::cmp::Ordering;

/// One indexed scalar. Ordering is total within a type; the catalog
/// guarantees a segment only ever holds one variant.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexValue {
    /// `TYPE i64`.
    I64(i64),
    /// `TYPE f64` (never NaN — coercion rejects it).
    F64(f64),
    /// `TYPE str` (raw bytes, memcmp order).
    Str(Vec<u8>),
}

impl Eq for IndexValue {}

impl Ord for IndexValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (IndexValue::I64(a), IndexValue::I64(b)) => a.cmp(b),
            (IndexValue::F64(a), IndexValue::F64(b)) => a.total_cmp(b),
            (IndexValue::Str(a), IndexValue::Str(b)) => a.cmp(b),
            // Cross-variant comparison means a catalog bug; order by
            // discriminant to stay total rather than panic in a
            // B-tree.
            (a, b) => disc(a).cmp(&disc(b)),
        }
    }
}

impl PartialOrd for IndexValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn disc(v: &IndexValue) -> u8 {
    match v {
        IndexValue::I64(_) => 0,
        IndexValue::F64(_) => 1,
        IndexValue::Str(_) => 2,
    }
}

impl IndexValue {
    /// Coerce raw field bytes per the declared type. `None` = the row
    /// is excluded from the index (and counted as a coerce failure).
    pub fn coerce(ty: crate::ValType, raw: &[u8]) -> Option<IndexValue> {
        match ty {
            // ANN kinds never coerce through IndexValue.
            crate::ValType::Vector => None,
            crate::ValType::I64 => std::str::from_utf8(raw)
                .ok()?
                .trim()
                .parse::<i64>()
                .ok()
                .map(IndexValue::I64),
            crate::ValType::F64 => {
                let f = std::str::from_utf8(raw).ok()?.trim().parse::<f64>().ok()?;
                if f.is_nan() {
                    return None;
                }
                Some(IndexValue::F64(f))
            }
            crate::ValType::Str => Some(IndexValue::Str(raw.to_vec())),
        }
    }

    /// Parse a query-side literal (same rules as [`Self::coerce`]).
    pub fn parse_literal(ty: crate::ValType, raw: &[u8]) -> Option<IndexValue> {
        Self::coerce(ty, raw)
    }

    /// Approximate heap bytes (for the memory formula / IDX.LIST).
    pub fn approx_bytes(&self) -> usize {
        match self {
            IndexValue::I64(_) | IndexValue::F64(_) => 8,
            IndexValue::Str(s) => s.len(),
        }
    }
}
