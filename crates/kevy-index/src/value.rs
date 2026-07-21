//! [`IndexValue`] — the three scalar types an index can hold, with a
//! total order (f64 via `total_cmp`, so NaN coerce-fails upstream and
//! never enters a segment).

use crate::catalog::ValType;
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

    /// Numeric view for aggregation (Str = 0.0; agg kinds only admit
    /// numeric types at CREATE, so this arm is unreachable there).
    pub fn as_f64(&self) -> f64 {
        match self {
            IndexValue::I64(v) => *v as f64,
            IndexValue::F64(v) => *v,
            IndexValue::Str(_) => 0.0,
        }
    }

    /// Approximate heap bytes (for the memory formula / IDX.LIST).
    pub fn approx_bytes(&self) -> usize {
        match self {
            IndexValue::I64(_) | IndexValue::F64(_) => 8,
            IndexValue::Str(s) => s.len(),
        }
    }
}

/// An order-preserving byte encoding of a coerced value.
///
/// Two values' encodings compare with `memcmp` exactly as the values
/// themselves compare. That is what lets `kevy-text` sort by a stored
/// value without learning what a number is: the caller encodes once per
/// candidate, and the segment compares bytes.
///
/// `None` when the raw bytes are not of that type — a document whose
/// stored value does not coerce has no place in the order, and is sorted
/// as missing rather than guessed at.
pub fn order_key(ty: ValType, raw: &[u8]) -> Option<Vec<u8>> {
    match IndexValue::coerce(ty, raw)? {
        // Bytes already compare as themselves.
        IndexValue::Str(v) => Some(v),
        // Flip the sign bit: two's complement negatives have the high bit
        // set and would otherwise sort above every positive.
        IndexValue::I64(v) => Some(((v as u64) ^ (1 << 63)).to_be_bytes().to_vec()),
        // The standard IEEE total-order transform. A negative float's
        // magnitude grows with its bit pattern, so inverting every bit
        // reverses that and drops it below the positives (whose sign bit
        // is set instead). Coercion rejects NaN, so there is none to
        // place.
        IndexValue::F64(v) => {
            let b = v.to_bits();
            let m = if b >> 63 == 1 { !b } else { b | (1 << 63) };
            Some(m.to_be_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod order_key_tests {
    use super::*;

    /// The encoding must agree with `IndexValue`'s own order on every
    /// pair — including across zero, which is where a naive big-endian
    /// encoding of a signed number gets it backwards.
    fn agrees(ty: ValType, raws: &[&str]) {
        let mut vals: Vec<(IndexValue, Vec<u8>)> = raws
            .iter()
            .map(|r| {
                (
                    IndexValue::coerce(ty, r.as_bytes()).expect("coerces"),
                    order_key(ty, r.as_bytes()).expect("encodes"),
                )
            })
            .collect();
        vals.sort_by(|a, b| a.1.cmp(&b.1));
        for w in vals.windows(2) {
            assert!(w[0].0 <= w[1].0, "{:?} then {:?} for {ty:?}", w[0].0, w[1].0);
        }
    }

    #[test]
    fn byte_order_matches_value_order() {
        agrees(ValType::I64, &["-9223372036854775808", "-5", "-1", "0", "1", "5", "9223372036854775807"]);
        agrees(ValType::F64, &["-1e308", "-1.5", "-0.5", "0", "0.5", "1.5", "1e308"]);
        agrees(ValType::Str, &["", "a", "ab", "b", "z"]);
    }

    #[test]
    fn a_value_that_does_not_coerce_has_no_key() {
        assert!(order_key(ValType::I64, b"cheap").is_none());
        assert!(order_key(ValType::F64, b"").is_none());
        assert_eq!(order_key(ValType::Str, b"anything"), Some(b"anything".to_vec()));
    }
}
