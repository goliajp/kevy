//! [`SegmentStats`] — the sizing and health counters a [`crate::Segment`]
//! reports.

/// Sizing + health counters (`IDX.LIST` / memory formula).
///
/// ```
/// use kevy_index::{Segment, SegmentStats};
/// let s = Segment::new().stats();
/// assert_eq!((s.entries, s.duplicates), (0, SegmentStats::default().duplicates));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SegmentStats {
    /// Live entries.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"a", None, Some(IndexValue::I64(1)));
    /// s.apply(b"a", Some(&IndexValue::I64(1)), Some(IndexValue::I64(2)));
    /// assert_eq!(s.stats().entries, 1, "a key holds one entry");
    /// ```
    pub entries: u64,
    /// Approximate heap bytes (the measured side of the documented
    /// memory formula).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"a", None, Some(IndexValue::Str(vec![b'x'; 1000])));
    /// assert!(s.stats().approx_bytes >= 1000);
    /// ```
    pub approx_bytes: u64,
    /// Rows excluded because the field failed coercion / was missing.
    ///
    /// ```
    /// # use kevy_index::{Segment, ValType, IndexValue};
    /// let mut s = Segment::new();
    /// s.apply(b"a", None, IndexValue::coerce(ValType::I64, b"not a number"));
    /// assert_eq!(s.stats().coerce_failures, 1);
    /// ```
    pub coerce_failures: u64,
    /// Values currently held by more than one key (unique fence).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// s.apply(b"a", None, Some(IndexValue::I64(7)));
    /// s.apply(b"b", None, Some(IndexValue::I64(7)));
    /// assert_eq!(s.stats().duplicates, 1);
    /// ```
    pub duplicates: u64,
}
