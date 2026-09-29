//! Per-group statistics and the one ranking order every face shares.

use crate::IndexValue;

/// One group's live statistics.
///
/// ```
/// use kevy_index::{GroupStats, IndexValue};
/// let mut total = GroupStats::default();
/// let mut part = GroupStats::default();
/// (part.count, part.sum, part.min, part.max) = (2, 10.0, Some(IndexValue::I64(3)), Some(IndexValue::I64(7)));
/// total.merge(&part);
/// assert_eq!(total.avg(), Some(5.0));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct GroupStats {
    /// Rows in the group.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// assert_eq!(s.group(b"eu").count, 2);
    /// s.apply(b"o:2", AggRow::Removed);
    /// assert_eq!(s.group(b"eu").count, 1);
    /// ```
    pub count: u64,
    /// Sum of the aggregated field (f64 accumulation — the i64
    /// overflow guard; precision bounds documented).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// assert_eq!(s.group(b"eu").sum, 42.0);
    /// ```
    pub sum: f64,
    /// Exact minimum (None only when count == 0).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// assert_eq!(s.group(b"eu").min, Some(IndexValue::I64(12)));
    /// // exact under deletion: the next-smallest value takes over
    /// s.apply(b"o:2", AggRow::Removed);
    /// assert_eq!(s.group(b"eu").min, Some(IndexValue::I64(30)));
    /// assert_eq!(s.group(b"us").min, None, "an unknown group has no rows");
    /// ```
    pub min: Option<IndexValue>,
    /// Exact maximum.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// assert_eq!(s.group(b"eu").max, Some(IndexValue::I64(30)));
    /// s.apply(b"o:1", row(b"eu", 5));
    /// assert_eq!(s.group(b"eu").max, Some(IndexValue::I64(12)));
    /// ```
    pub max: Option<IndexValue>,
}

impl GroupStats {
    /// Derived average.
    ///
    /// ```
    /// assert_eq!(kevy_index::GroupStats::default().avg(), None);
    /// ```
    pub fn avg(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// This group's standing under `by`, oriented so a larger score ranks
    /// higher for every metric — the order [`sort_groups`] puts groups in
    /// (a smaller minimum ranks higher; a group without one ranks last).
    ///
    /// ```
    /// use kevy_index::{AggBy, GroupStats, IndexValue};
    /// let mut g = GroupStats::default();
    /// (g.count, g.min) = (3, Some(IndexValue::I64(2)));
    /// assert_eq!((g.rank_score(AggBy::Count), g.rank_score(AggBy::Min)), (3.0, -2.0));
    /// assert_eq!(g.rank_score(AggBy::Max), f64::NEG_INFINITY);
    /// ```
    pub fn rank_score(&self, by: AggBy) -> f64 {
        match by {
            AggBy::Count => self.count as f64,
            AggBy::Sum => self.sum,
            AggBy::Max => self.max.as_ref().map_or(f64::NEG_INFINITY, IndexValue::as_f64),
            AggBy::Min => self.min.as_ref().map_or(f64::NEG_INFINITY, |v| -v.as_f64()),
        }
    }

    /// Fold one shard's partial for the same group into this one (reduce
    /// side): counts and sums add, min and max take the extremes.
    ///
    /// ```
    /// use kevy_index::{GroupStats, IndexValue};
    /// let mut a = GroupStats::default();
    /// a.count = 1;
    /// a.min = Some(IndexValue::I64(5));
    /// let mut b = GroupStats::default();
    /// b.count = 1;
    /// b.min = Some(IndexValue::I64(2));
    /// a.merge(&b);
    /// assert_eq!((a.count, a.min), (2, Some(IndexValue::I64(2))));
    /// ```
    pub fn merge(&mut self, part: &GroupStats) {
        self.count += part.count;
        self.sum += part.sum;
        self.min = match (self.min.take(), part.min.clone()) {
            (Some(a), Some(b)) => Some(if b < a { b } else { a }),
            (a, b) => a.or(b),
        };
        self.max = match (self.max.take(), part.max.clone()) {
            (Some(a), Some(b)) => Some(if b > a { b } else { a }),
            (a, b) => a.or(b),
        };
    }
}

/// Ranking metric for [`AggSegment::top_groups`](crate::AggSegment::top_groups).
///
/// ```
/// use kevy_index::AggBy;
/// assert_eq!(AggBy::default(), AggBy::Count);
/// assert_eq!(AggBy::parse(b"MAX"), Some(AggBy::Max), "tags are case-insensitive");
/// assert_eq!(AggBy::parse(b"avg"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum AggBy {
    /// By row count (default).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:3", row(b"us", 99));
    /// let top = s.top_groups(kevy_index::AggBy::Count, 1);
    /// assert_eq!((top[0].0.as_slice(), top[0].1.count), (&b"eu"[..], 2));
    /// ```
    #[default]
    Count,
    /// By sum.
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:3", row(b"us", 99));
    /// let top = s.top_groups(kevy_index::AggBy::Sum, 1);
    /// assert_eq!((top[0].0.as_slice(), top[0].1.sum), (&b"us"[..], 99.0));
    /// ```
    Sum,
    /// By minimum (ascending — smallest mins first).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:3", row(b"us", 99));
    /// let top = s.top_groups(kevy_index::AggBy::Min, 2);
    /// assert_eq!(top[0].0, b"eu", "12 < 99");
    /// ```
    Min,
    /// By maximum (descending — largest maxes first).
    ///
    /// ```
    /// # use kevy_index::{AggRow, AggSegment, IndexValue};
    /// # let mut s = AggSegment::new();
    /// # let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
    /// s.apply(b"o:1", row(b"eu", 30));
    /// s.apply(b"o:2", row(b"eu", 12));
    /// s.apply(b"o:3", row(b"us", 99));
    /// let top = s.top_groups(kevy_index::AggBy::Max, 2);
    /// assert_eq!(top[0].0, b"us", "99 > 30");
    /// ```
    Max,
}

impl AggBy {
    /// The wire tag, as [`AggBy::parse`] reads it.
    ///
    /// ```
    /// use kevy_index::AggBy;
    /// assert_eq!(AggBy::parse(AggBy::Sum.tag().as_bytes()), Some(AggBy::Sum));
    /// ```
    pub fn tag(self) -> &'static str {
        match self {
            AggBy::Count => "count",
            AggBy::Sum => "sum",
            AggBy::Min => "min",
            AggBy::Max => "max",
        }
    }

    /// Wire tag.
    pub fn parse(raw: &[u8]) -> Option<AggBy> {
        if raw.eq_ignore_ascii_case(b"count") {
            Some(AggBy::Count)
        } else if raw.eq_ignore_ascii_case(b"sum") {
            Some(AggBy::Sum)
        } else if raw.eq_ignore_ascii_case(b"min") {
            Some(AggBy::Min)
        } else if raw.eq_ignore_ascii_case(b"max") {
            Some(AggBy::Max)
        } else {
            None
        }
    }
}

/// Shared ranking order (per-shard AND at the reduce after merging
/// shard partials — one definition, no drift).
///
/// ```
/// use kevy_index::{AggBy, AggRow, AggSegment, IndexValue, sort_groups};
/// let row = |g: &[u8], v| AggRow::Member { group: g.to_vec(), value: IndexValue::I64(v) };
/// let (mut a, mut b) = (AggSegment::new(), AggSegment::new());
/// a.apply(b"o:1", row(b"eu", 5));
/// b.apply(b"o:2", row(b"us", 9));
/// b.apply(b"o:3", row(b"us", 1));
/// // reduce: concatenate the shards' unranked chunks, then rank once
/// let mut all = a.all_groups();
/// all.extend(b.all_groups());
/// sort_groups(&mut all, AggBy::Sum);
/// assert_eq!(all[0].0, b"us");
/// sort_groups(&mut all, AggBy::Min);
/// assert_eq!(all[0].0, b"us", "1 is the smallest minimum");
/// ```
pub fn sort_groups(all: &mut [(Vec<u8>, GroupStats)], by: AggBy) {
    match by {
        AggBy::Count => all.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0))),
        AggBy::Sum => all.sort_by(|a, b| b.1.sum.total_cmp(&a.1.sum).then_with(|| a.0.cmp(&b.0))),
        AggBy::Min => all.sort_by(|a, b| {
            match (&a.1.min, &b.1.min) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| a.0.cmp(&b.0))
        }),
        AggBy::Max => all.sort_by(|a, b| {
            match (&b.1.max, &a.1.max) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| a.0.cmp(&b.0))
        }),
    }
}
