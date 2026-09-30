//! The clause-carrying scalar query — `FILTER` / `SORT` / `DISTINCT` /
//! `FACET` / `OFFSET` over one shard's [`Segment`] — and the origin-side
//! merge every embedding runs the same way.
//!
//! The semantics mirror the text surface exactly (kevy-text's
//! `segment_query` / `segment_select`), re-stated here for the scalar
//! kinds:
//!
//! * `FILTER` — non-scoring predicates, ANDed; a candidate without the
//!   stored value FAILS (absent is not a value).
//! * `SORT` — selection by the stored value's order key; a row with no
//!   usable value sorts LAST in both directions; ties break by row key.
//! * `DISTINCT` — at most one row per coerced value identity, collapsed
//!   DURING selection; a row with no value is its own group.
//! * `FACET` — value counts over the WHOLE match set (after `FILTER`,
//!   before any truncation; `DISTINCT` does not reduce them).
//! * `OFFSET` — applied at the origin over the merged page; each shard
//!   returns `limit + offset`.

use std::collections::HashMap;

use crate::catalog::ValType;
use crate::segment::{Cursor, Segment};
use crate::value::IndexValue;
use crate::value::ValueTest;
use kevy_text::{SortOrder, sorted_order};

use crate::segment_claused_merge::finish_facets;
pub use crate::segment_claused_merge::{
    ColdEntryRow, claused_over, fold_facets, merge_claused, sort_facets, values_pass,
};

/// Everything a scalar query carries beyond its bounds. Field indices
/// are positions into the spec's declared `VALUES` list; the caller
/// resolves names (and errors on unknown ones) before building this.
///
/// ```
/// use kevy_index::{ScalarClauses, SortOrder, ValType};
/// let c = ScalarClauses::new(10).with_sort(0, SortOrder::Desc, ValType::I64);
/// assert!(c.selects());
/// assert!(!ScalarClauses::new(10).selects());
/// ```
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ScalarClauses<'a> {
    /// `(stored-value position, typed test)` per `FILTER`, ANDed.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest};
    /// let mut s = Segment::with_values(1);
    /// for (k, v, city) in [(b"a", 1, "kyoto"), (b"b", 2, "osaka"), (b"c", 3, "kyoto")] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(city.as_bytes())]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let f = [(0, ValueTest::eq(ValType::Str, b"kyoto").expect("a str test"))];
    /// let page = s.query_claused(&lo, &hi, None, &ScalarClauses::new(10).with_filters(&f));
    /// let keys: Vec<_> = page.hits.iter().map(|h| h.key.as_slice()).collect();
    /// assert_eq!(keys, [b"a", b"c"]);
    /// ```
    pub filters: &'a [(usize, ValueTest)],
    /// `SORT`: `(position, direction, declared type)`.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest};
    /// let mut s = Segment::with_values(1);
    /// for (k, v, city) in [(b"a", 1, "kyoto"), (b"b", 2, "osaka"), (b"c", 3, "kyoto")] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(city.as_bytes())]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let c = ScalarClauses::new(10).with_sort(0, SortOrder::Desc, ValType::Str);
    /// let page = s.query_claused(&lo, &hi, None, &c);
    /// assert_eq!(page.hits[0].key, b"b", "osaka sorts first descending");
    /// ```
    pub sort: Option<(usize, SortOrder, ValType)>,
    /// `DISTINCT`: `(position, declared type)`.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest};
    /// let mut s = Segment::with_values(1);
    /// for (k, v, city) in [(b"a", 1, "kyoto"), (b"b", 2, "osaka"), (b"c", 3, "kyoto")] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(city.as_bytes())]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let c = ScalarClauses::new(10).with_distinct(0, ValType::Str);
    /// assert_eq!(s.query_claused(&lo, &hi, None, &c).hits.len(), 2, "one row per city");
    /// ```
    pub distinct: Option<(usize, ValType)>,
    /// `FACET`: `(position, declared type)` per requested field.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest};
    /// let mut s = Segment::with_values(1);
    /// for (k, v, city) in [(b"a", 1, "kyoto"), (b"b", 2, "osaka"), (b"c", 3, "kyoto")] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(city.as_bytes())]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let f = [(0, ValType::Str)];
    /// let page = s.query_claused(&lo, &hi, None, &ScalarClauses::new(1).with_facets(&f));
    /// assert_eq!((page.facets[0][0].1.as_slice(), page.facets[0][0].2), (&b"kyoto"[..], 2));
    /// ```
    pub facets: &'a [(usize, ValType)],
    /// How many hits this shard returns (`limit + offset` — the origin
    /// drains the offset after the merge).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType, ValueTest};
    /// let mut s = Segment::with_values(1);
    /// for (k, v, city) in [(b"a", 1, "kyoto"), (b"b", 2, "osaka"), (b"c", 3, "kyoto")] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(city.as_bytes())]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let page = s.query_claused(&lo, &hi, None, &ScalarClauses::new(2));
    /// assert_eq!(page.hits.len(), 2);
    /// ```
    pub fetch: usize,
}

impl<'a> ScalarClauses<'a> {
    /// No clauses, returning at most `fetch` hits (`limit + offset`).
    ///
    /// ```
    /// let c = kevy_index::ScalarClauses::new(25);
    /// assert_eq!((c.fetch, c.filters.len(), c.sort), (25, 0, None));
    /// ```
    pub fn new(fetch: usize) -> Self {
        ScalarClauses { filters: &[], sort: None, distinct: None, facets: &[], fetch }
    }

    /// These `FILTER` predicates, ANDed.
    ///
    /// ```
    /// use kevy_index::{ScalarClauses, ValType, ValueTest};
    /// let f = [(0, ValueTest::eq(ValType::I64, b"1").expect("a test"))];
    /// assert_eq!(ScalarClauses::new(5).with_filters(&f).filters.len(), 1);
    /// ```
    #[must_use]
    pub fn with_filters(mut self, filters: &'a [(usize, ValueTest)]) -> Self {
        self.filters = filters;
        self
    }

    /// `SORT` by stored value `position` in `order`, compared as `ty`.
    ///
    /// ```
    /// use kevy_index::{ScalarClauses, SortOrder, ValType};
    /// let c = ScalarClauses::new(5).with_sort(1, SortOrder::Asc, ValType::Str);
    /// assert_eq!(c.sort, Some((1, SortOrder::Asc, ValType::Str)));
    /// ```
    #[must_use]
    pub fn with_sort(mut self, position: usize, order: SortOrder, ty: ValType) -> Self {
        self.sort = Some((position, order, ty));
        self
    }

    /// `DISTINCT` on stored value `position`, identified as `ty`.
    ///
    /// ```
    /// use kevy_index::{ScalarClauses, ValType};
    /// assert_eq!(ScalarClauses::new(5).with_distinct(0, ValType::I64).distinct, Some((0, ValType::I64)));
    /// ```
    #[must_use]
    pub fn with_distinct(mut self, position: usize, ty: ValType) -> Self {
        self.distinct = Some((position, ty));
        self
    }

    /// `FACET` counts for these `(position, type)` fields.
    ///
    /// ```
    /// use kevy_index::{ScalarClauses, ValType};
    /// let f = [(0, ValType::Str)];
    /// assert!(ScalarClauses::new(5).with_facets(&f).selects());
    /// ```
    #[must_use]
    pub fn with_facets(mut self, facets: &'a [(usize, ValType)]) -> Self {
        self.facets = facets;
        self
    }

    /// Whether any clause reshapes selection (vs FILTER, which only
    /// thins the driving order and stays cursor-compatible).
    pub fn selects(&self) -> bool {
        self.sort.is_some() || self.distinct.is_some() || !self.facets.is_empty()
    }
}

/// One selected row: its key and indexed value, plus the sort /
/// distinct keys the origin merge needs (only present when the query
/// carried the clause).
///
/// ```
/// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType};
/// let mut s = Segment::with_values(1);
/// s.apply_with_values(b"k", None, Some(IndexValue::I64(7)), &[Some(b"42")]);
/// let c = ScalarClauses::new(5).with_sort(0, SortOrder::Asc, ValType::I64);
/// let hit = &s.query_claused(&IndexValue::I64(0), &IndexValue::I64(9), None, &c).hits[0];
/// assert_eq!((hit.key.as_slice(), &hit.value), (&b"k"[..], &IndexValue::I64(7)));
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ScalarHit {
    /// Row key.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(7)), &[Some(b"42")]);
    /// let c = ScalarClauses::new(5).with_sort(0, SortOrder::Asc, ValType::I64);
    /// let hit = &s.query_claused(&IndexValue::I64(0), &IndexValue::I64(9), None, &c).hits[0];
    /// assert_eq!(hit.key, b"k");
    /// ```
    pub key: Vec<u8>,
    /// The indexed (driving) value.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(7)), &[Some(b"42")]);
    /// let c = ScalarClauses::new(5).with_sort(0, SortOrder::Asc, ValType::I64);
    /// let hit = &s.query_claused(&IndexValue::I64(0), &IndexValue::I64(9), None, &c).hits[0];
    /// assert_eq!(hit.value, IndexValue::I64(7), "the driving value, not the sort field");
    /// ```
    pub value: IndexValue,
    /// The sort field's order-preserving key (`None` = no usable value).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(7)), &[Some(b"42")]);
    /// let c = ScalarClauses::new(5).with_sort(0, SortOrder::Asc, ValType::I64);
    /// let hit = &s.query_claused(&IndexValue::I64(0), &IndexValue::I64(9), None, &c).hits[0];
    /// assert_eq!(hit.okey, kevy_index::order_key(ValType::I64, b"42"));
    /// ```
    pub okey: Option<Vec<u8>>,
    /// The distinct field's coerced identity (`None` = own group).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, SortOrder, ValType};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(7)), &[Some(b"42")]);
    /// let c = ScalarClauses::new(5).with_sort(0, SortOrder::Asc, ValType::I64);
    /// let hit = &s.query_claused(&IndexValue::I64(0), &IndexValue::I64(9), None, &c).hits[0];
    /// assert_eq!(hit.dkey, None, "only a DISTINCT query gives a hit an identity");
    /// ```
    pub dkey: Option<Vec<u8>>,
}

impl ScalarHit {
    /// A hit for row `key` under indexed value `value`, carrying no sort
    /// or distinct key — the shape a query without those clauses returns,
    /// and where a merge that decodes shard replies starts.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ScalarHit};
    /// let mut h = ScalarHit::new(b"k".to_vec(), IndexValue::I64(1));
    /// h.okey = Some(b"a".to_vec());
    /// assert_eq!((h.okey.as_deref(), h.dkey), (Some(&b"a"[..]), None));
    /// ```
    pub fn new(key: Vec<u8>, value: IndexValue) -> ScalarHit {
        ScalarHit { key, value, okey: None, dkey: None }
    }
}

/// One facet bucket: the identity a cross-shard merge sums by, a
/// spelling that occurs in the corpus, and the count.
///
/// ```
/// use kevy_index::{FacetBucket, ValType, fold_facets, order_key};
/// let id = order_key(ValType::F64, b"1").expect("a number");
/// let mut total: Vec<Vec<FacetBucket>> = vec![vec![(id.clone(), b"1".to_vec(), 2)]];
/// fold_facets(&mut total, vec![vec![(id, b"1.0".to_vec(), 3)]]);
/// assert_eq!(total[0][0].1, b"1", "the first label seen is kept");
/// assert_eq!(total[0][0].2, 5, "counts sum by identity");
/// ```
pub type FacetBucket = (Vec<u8>, Vec<u8>, u64);

/// One facet field's in-flight counts: identity → (label, count).
pub(crate) type FacetCounts = HashMap<Vec<u8>, (Vec<u8>, u64)>;

/// One shard's clause-carrying page.
///
/// ```
/// # use kevy_index::{IndexValue, ScalarClauses, Segment, ValType};
/// let mut s = Segment::with_values(1);
/// for (k, v) in [(b"a", 1), (b"b", 2), (b"c", 3)] {
///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(b"x")]);
/// }
/// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
/// let page = s.query_claused(&lo, &hi, None, &ScalarClauses::new(2));
/// assert_eq!((page.hits.len(), page.facets.len(), page.cursor.is_some()), (2, 0, true));
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ClausedPage {
    /// The selected hits (driving order, or sort order under `SORT`).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, ValType};
    /// let mut s = Segment::with_values(1);
    /// for (k, v) in [(b"a", 1), (b"b", 2), (b"c", 3)] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(b"x")]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let page = s.query_claused(&lo, &hi, None, &ScalarClauses::new(10));
    /// let values: Vec<_> = page.hits.iter().map(|h| h.value.clone()).collect();
    /// assert_eq!(values, [IndexValue::I64(1), IndexValue::I64(2), IndexValue::I64(3)]);
    /// ```
    pub hits: Vec<ScalarHit>,
    /// Per requested facet field, its buckets over this shard's match
    /// set.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, ValType};
    /// let mut s = Segment::with_values(1);
    /// for (k, v) in [(b"a", 1), (b"b", 2), (b"c", 3)] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(b"x")]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let f = [(0, ValType::Str)];
    /// let page = s.query_claused(&lo, &hi, None, &ScalarClauses::new(1).with_facets(&f));
    /// assert_eq!(page.facets[0][0].2, 3, "facets count the whole match set, not the page");
    /// ```
    pub facets: Vec<Vec<FacetBucket>>,
    /// Resume cursor — only ever `Some` on the FILTER-with-CURSOR path
    /// (selection clauses refuse cursors at the surface).
    ///
    /// ```
    /// # use kevy_index::{IndexValue, ScalarClauses, Segment, ValType};
    /// let mut s = Segment::with_values(1);
    /// for (k, v) in [(b"a", 1), (b"b", 2), (b"c", 3)] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(b"x")]);
    /// }
    /// let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(9));
    /// let c = ScalarClauses::new(2);
    /// let first = s.query_claused(&lo, &hi, None, &c);
    /// let rest = s.query_claused(&lo, &hi, first.cursor.as_ref(), &c);
    /// assert_eq!(rest.hits[0].key, b"c");
    /// assert!(rest.cursor.is_none(), "exhausted");
    /// ```
    pub cursor: Option<Cursor>,
}

impl Segment {
    /// The clause-carrying count of `[min, max]`: the full walk with
    /// the FILTER predicates applied, materializing nothing — the
    /// total a claused query would reach, without building pages. The
    /// consumer shape this closes: counting a filtered axis used to
    /// mean fetching every page and taking `len`.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment, ValType, ValueTest};
    /// let mut s = Segment::with_values(1);
    /// for (k, v, c) in [(b"a", 1, "x"), (b"b", 2, "y"), (b"c", 3, "x")] {
    ///     s.apply_with_values(k, None, Some(IndexValue::I64(v)), &[Some(c.as_bytes())]);
    /// }
    /// let f = [(0, ValueTest::eq(ValType::Str, b"x").expect("a str test"))];
    /// assert_eq!(s.count_claused(&IndexValue::I64(0), &IndexValue::I64(9), &f), 2);
    /// ```
    pub fn count_claused(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        filters: &[(usize, ValueTest)],
    ) -> u64 {
        if filters.is_empty() {
            return self.count(min, max);
        }
        let mut w = self.range_walk(min, max, None);
        let mut buf = Vec::new();
        let mut n = 0;
        while w.advance() {
            n += u64::from(passes(&w, filters, &mut buf));
        }
        n
    }

    /// The clause-carrying scan of `[min, max]`. FILTER-only queries
    /// stream in driving order and stay cursor-paged; any selection
    /// clause walks deeper (the whole range for `SORT` / `FACET`) and
    /// returns no cursor.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ScalarClauses, Segment};
    /// let mut s = Segment::with_values(1);
    /// s.apply_with_values(b"k", None, Some(IndexValue::I64(1)), &[Some(b"x")]);
    /// let page = s.query_claused(&IndexValue::I64(0), &IndexValue::I64(9), None, &ScalarClauses::new(5));
    /// assert_eq!(page.hits.len(), 1);
    /// ```
    pub fn query_claused(
        &self,
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        c: &ScalarClauses<'_>,
    ) -> ClausedPage {
        let mut facets: Vec<FacetCounts> = vec![HashMap::new(); c.facets.len()];
        let mut hits: Vec<ScalarHit> = Vec::new();
        let mut groups: HashMap<Vec<u8>, usize> = HashMap::new();
        // Selection needs the whole match set when sorting (top-K by the
        // sort key) or faceting (counts before truncation); otherwise
        // the walk stops as soon as the page is full.
        let full_walk = c.sort.is_some() || !c.facets.is_empty();
        let mut w = self.range_walk(min, max, cursor);
        let mut buf = Vec::new();
        while w.advance() {
            if !passes(&w, c.filters, &mut buf) {
                continue;
            }
            count_facets(&w, c, &mut facets, &mut buf);
            if !full_walk && hits.len() == c.fetch {
                break;
            }
            select_hit(&mut w, c, &mut hits, &mut groups, &mut buf);
        }
        if let Some((_, order, _)) = c.sort {
            hits.sort_by(|a, b| {
                sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), order)
            });
        }
        hits.truncate(c.fetch);
        let cursor = filter_cursor(c, &hits);
        ClausedPage { hits, facets: finish_facets(facets), cursor }
    }
}

#[path = "segment_claused_select.rs"]
mod select;
use select::{count_facets, filter_cursor, passes, select_hit};

#[cfg(test)]
#[path = "segment_claused_tests.rs"]
mod tests;
