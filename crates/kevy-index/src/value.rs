//! [`IndexValue`] — the three scalar types an index can hold, with a
//! total order (f64 via `total_cmp`, so NaN coerce-fails upstream and
//! never enters a segment).

use crate::catalog::ValType;
use std::cmp::Ordering;

/// One indexed scalar. Ordering is total within a type; the catalog
/// guarantees a segment only ever holds one variant.
///
/// Values enter through [`coerce`](Self::coerce), which applies the
/// declared type to a field's raw bytes; a row whose field does not
/// coerce is left out of the index.
///
/// ```
/// use kevy_index::{IndexValue, ValType};
///
/// let age = IndexValue::coerce(ValType::I64, b" 41 ");
/// assert_eq!(age, Some(IndexValue::I64(41)));
/// assert_eq!(IndexValue::coerce(ValType::I64, b"forty"), None);
/// assert_eq!(IndexValue::coerce(ValType::F64, b"NaN"), None);
/// assert_eq!(IndexValue::parse_literal(ValType::F64, b"2.5"), Some(IndexValue::F64(2.5)));
///
/// assert_eq!(IndexValue::I64(3).as_f64(), 3.0);
/// assert_eq!(IndexValue::Str(b"kyoto".to_vec()).approx_bytes(), 5);
/// assert!(IndexValue::F64(-1.0) < IndexValue::F64(0.5));
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum IndexValue {
    /// `TYPE i64`.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// assert_eq!(IndexValue::coerce(ValType::I64, b"-7"), Some(IndexValue::I64(-7)));
    /// ```
    I64(i64),
    /// `TYPE f64` (never NaN — coercion rejects it).
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// assert_eq!(IndexValue::coerce(ValType::F64, b"1e3"), Some(IndexValue::F64(1000.0)));
    /// ```
    F64(f64),
    /// `TYPE str` (raw bytes, memcmp order).
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// let v = IndexValue::coerce(ValType::Str, b"Zed").unwrap();
    /// assert_eq!(v, IndexValue::Str(b"Zed".to_vec()));
    /// assert!(v < IndexValue::Str(b"apple".to_vec()), "bytes compare, not letters");
    /// ```
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
            crate::ValType::I64 => {
                std::str::from_utf8(raw).ok()?.trim().parse::<i64>().ok().map(IndexValue::I64)
            }
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

    /// The text a reply shows for this value: decimal for `i64`, Rust's
    /// shortest round-trip form for `f64`, the raw bytes for `str`.
    ///
    /// ```
    /// use kevy_index::IndexValue;
    /// assert_eq!(IndexValue::I64(-7).render(), b"-7");
    /// assert_eq!(IndexValue::F64(2.5).render(), b"2.5");
    /// assert_eq!(IndexValue::Str(b"kyoto".to_vec()).render(), b"kyoto");
    /// ```
    pub fn render(&self) -> Vec<u8> {
        match self {
            IndexValue::I64(i) => i.to_string().into_bytes(),
            IndexValue::F64(f) => format!("{f}").into_bytes(),
            IndexValue::Str(s) => s.clone(),
        }
    }

    /// Append the tagged binary form cursors and shard replies carry:
    /// a tag byte (0 `i64`, 1 `f64`, 2 `str`), then the value — eight
    /// little-endian bytes, or a `u32` little-endian length and the bytes.
    ///
    /// ```
    /// use kevy_index::IndexValue;
    /// let mut out = Vec::new();
    /// IndexValue::I64(1).encode(&mut out);
    /// assert_eq!(out, [0, 1, 0, 0, 0, 0, 0, 0, 0]);
    /// ```
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            IndexValue::I64(i) => {
                out.push(0);
                out.extend_from_slice(&i.to_le_bytes());
            }
            IndexValue::F64(f) => {
                out.push(1);
                out.extend_from_slice(&f.to_le_bytes());
            }
            IndexValue::Str(s) => {
                out.push(2);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s);
            }
        }
    }

    /// Read one [`encode`](Self::encode)d value at `*pos`, advancing
    /// `pos` past it; `None` on an unknown tag or truncated bytes.
    ///
    /// ```
    /// use kevy_index::IndexValue;
    /// let mut buf = Vec::new();
    /// IndexValue::Str(b"ab".to_vec()).encode(&mut buf);
    /// IndexValue::F64(0.5).encode(&mut buf);
    /// let mut pos = 0;
    /// assert_eq!(IndexValue::decode(&buf, &mut pos), Some(IndexValue::Str(b"ab".to_vec())));
    /// assert_eq!(IndexValue::decode(&buf, &mut pos), Some(IndexValue::F64(0.5)));
    /// assert_eq!((pos, IndexValue::decode(&buf, &mut pos)), (buf.len(), None));
    /// ```
    pub fn decode(b: &[u8], pos: &mut usize) -> Option<IndexValue> {
        let tag = *b.get(*pos)?;
        let body = pos.checked_add(1)?;
        let (v, end) = match tag {
            0 => (
                IndexValue::I64(i64::from_le_bytes(b.get(body..body + 8)?.try_into().ok()?)),
                body + 8,
            ),
            1 => (
                IndexValue::F64(f64::from_le_bytes(b.get(body..body + 8)?.try_into().ok()?)),
                body + 8,
            ),
            2 => {
                let n = u32::from_le_bytes(b.get(body..body + 4)?.try_into().ok()?) as usize;
                let start = body + 4;
                (IndexValue::Str(b.get(start..start.checked_add(n)?)?.to_vec()), start + n)
            }
            _ => return None,
        };
        *pos = end;
        Some(v)
    }
}

/// A comparison over a stored value's raw bytes, built once from the
/// type the field was declared as.
///
/// `EQ` is the degenerate range `[v, v]`: stored values are totally
/// ordered, so equality needs no second code path — and one path cannot
/// disagree with itself about what a bound means.
///
/// ```
/// use kevy_index::{ValType, ValueTest};
///
/// let adults = ValueTest::range(ValType::I64, b"18", b"64").unwrap();
/// assert!(adults.passes(b"41"));
/// assert!(!adults.passes(b"70"));
/// assert!(!adults.passes(b"n/a"), "text in a numeric field is in no range");
///
/// let kyoto = ValueTest::eq(ValType::Str, b"kyoto").unwrap();
/// assert!(kyoto.passes(b"kyoto") && !kyoto.passes(b"osaka"));
///
/// // query bounds may be time expressions on i64 fields
/// let now = 1_000_000;
/// let last_day = ValueTest::range_at(ValType::I64, b"@now-1d", b"@now", now).unwrap();
/// assert!(last_day.passes(b"999000") && !last_day.passes(b"1"));
/// assert!(ValueTest::eq_at(ValType::I64, b"@now", now).unwrap().passes(b"1000000"));
///
/// assert_eq!(ValueTest::range(ValType::I64, b"1", b"ten"), None);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ValueTest {
    ty: ValType,
    lo: IndexValue,
    hi: IndexValue,
}

/// One query BOUND's value: [`IndexValue::parse_literal`] plus the
/// `@` time expressions on i64 fields (`@now`, `@now-7d`,
/// a calendar literal, per [`kevy_time::eval`]) — only ever called on
/// bound bytes, never on row data (a row whose field holds "@now" is
/// data, not an expression, and the write path never comes here).
/// Non-i64 fields pass through untouched, so a str field matching a
/// literal "@…" value stays unambiguous.
///
/// ```
/// use kevy_index::{IndexValue, ValType, parse_literal_bound};
///
/// let now = 86_400; // seconds
/// assert_eq!(parse_literal_bound(ValType::I64, b"@now", now), Some(IndexValue::I64(now)));
/// assert_eq!(parse_literal_bound(ValType::I64, b"@now-1d", now), Some(IndexValue::I64(0)));
/// assert_eq!(parse_literal_bound(ValType::I64, b"12", now), Some(IndexValue::I64(12)));
/// // on a str field "@now" is just text
/// assert_eq!(
///     parse_literal_bound(ValType::Str, b"@now", now),
///     Some(IndexValue::Str(b"@now".to_vec()))
/// );
/// ```
pub fn parse_literal_bound(ty: ValType, raw: &[u8], now: i64) -> Option<IndexValue> {
    if ty == ValType::I64 && raw.first() == Some(&b'@') {
        return kevy_time::eval(raw, now).map(IndexValue::I64);
    }
    IndexValue::parse_literal(ty, raw)
}

/// [`parse_literal_bound`]'s coercing sibling for FILTER bounds.
///
/// ```
/// use kevy_index::{IndexValue, ValType, coerce_bound};
///
/// let now = 5_000;
/// assert_eq!(coerce_bound(ValType::I64, b"@now", now), Some(IndexValue::I64(5_000)));
/// assert_eq!(coerce_bound(ValType::F64, b" 0.25 ", now), Some(IndexValue::F64(0.25)));
/// assert_eq!(coerce_bound(ValType::I64, b"soon", now), None);
/// ```
pub fn coerce_bound(ty: ValType, raw: &[u8], now: i64) -> Option<IndexValue> {
    if ty == ValType::I64 && raw.first() == Some(&b'@') {
        return kevy_time::eval(raw, now).map(IndexValue::I64);
    }
    IndexValue::coerce(ty, raw)
}

impl ValueTest {
    /// `RANGE min max` on a field declared as `ty`. `None` when a bound
    /// is not of that type — a bound the index cannot interpret is an
    /// error, not an empty result.
    pub fn range(ty: ValType, min: &[u8], max: &[u8]) -> Option<ValueTest> {
        Some(ValueTest { ty, lo: IndexValue::coerce(ty, min)?, hi: IndexValue::coerce(ty, max)? })
    }

    /// [`ValueTest::range`] for query bounds: `@` time expressions
    /// resolve against `now` on i64 fields.
    pub fn range_at(ty: ValType, min: &[u8], max: &[u8], now: i64) -> Option<ValueTest> {
        Some(ValueTest { ty, lo: coerce_bound(ty, min, now)?, hi: coerce_bound(ty, max, now)? })
    }

    /// `EQ v` on a field declared as `ty`.
    pub fn eq(ty: ValType, v: &[u8]) -> Option<ValueTest> {
        let v = IndexValue::coerce(ty, v)?;
        Some(ValueTest { ty, lo: v.clone(), hi: v })
    }

    /// [`ValueTest::eq`] for query bounds: `@` time expressions
    /// resolve against `now` on i64 fields.
    pub fn eq_at(ty: ValType, v: &[u8], now: i64) -> Option<ValueTest> {
        let v = coerce_bound(ty, v, now)?;
        Some(ValueTest { ty, lo: v.clone(), hi: v })
    }

    /// Whether a stored value's bytes satisfy the test.
    ///
    /// A value that does not coerce fails: text sitting in a field
    /// declared numeric is not inside any numeric range, and passing it
    /// would be the accept-and-ignore shape this surface keeps refusing.
    pub fn passes(&self, raw: &[u8]) -> bool {
        IndexValue::coerce(self.ty, raw).is_some_and(|v| v >= self.lo && v <= self.hi)
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
///
/// ```
/// use kevy_index::{ValType, order_key};
///
/// let neg = order_key(ValType::I64, b"-5").unwrap();
/// let pos = order_key(ValType::I64, b"3").unwrap();
/// assert!(neg < pos, "memcmp order matches numeric order");
///
/// let small = order_key(ValType::F64, b"-0.5").unwrap();
/// let big = order_key(ValType::F64, b"10").unwrap();
/// assert!(small < big);
///
/// assert_eq!(order_key(ValType::Str, b"abc"), Some(b"abc".to_vec()));
/// assert_eq!(order_key(ValType::I64, b"abc"), None);
/// ```
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
        agrees(
            ValType::I64,
            &["-9223372036854775808", "-5", "-1", "0", "1", "5", "9223372036854775807"],
        );
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
