//! The optional clauses an embedded index query carries — the in-process
//! twin of the wire's `FILTER` / `SORT` / `DISTINCT` / `FACET` / `OFFSET`
//! (and, for text MATCH, `HIGHLIGHT` / `TYPO` / `IN`). A `#[path]` child
//! of `ops_index.rs`, split out for the 500-LOC house rule.

use kevy_index::SortOrder;

/// One `FILTER` predicate: which stored value field it reads, and the
/// test on it — the wire's `RANGE` / `EQ` shapes, in-process.
///
/// The bounds are raw bytes and are coerced with the type the field was
/// DECLARED as, so a numeric range compares numerically rather than
/// lexicographically.
///
/// ```
/// use kevy_embedded::ValueFilter;
///
/// let cheap = ValueFilter::Range { field: b"price", min: b"0", max: b"10" };
/// assert_ne!(cheap, ValueFilter::Eq { field: b"price", value: b"5" });
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValueFilter<'a> {
    /// `field` between `min` and `max`, both inclusive.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let f = [ValueFilter::Range { field: b"total", min: b"5", max: b"10" }];
    /// assert_eq!(keys(ScalarQueryOpts::default().with_filters(&f))?, [b"u:1".to_vec(), b"u:3".to_vec()]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    Range {
        /// The declared value field to read.
        ///
        /// ```
        /// # use kevy_embedded::*;
        /// # let s = Store::open(Config::default())?;
        /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
        /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
        /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
        /// # for (k, age, status, total) in rows {
        /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
        /// # }
        /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
        /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
        /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
        /// # };
        /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
        /// // `total` is declared I64, so the bounds compare as numbers, not bytes
        /// let f = [ValueFilter::Range { field: b"total", min: b"9", max: b"100" }];
        /// assert_eq!(keys(ScalarQueryOpts::default().with_filters(&f))?, [b"u:2".to_vec()]);
        /// # Ok::<(), kevy_embedded::KevyError>(())
        /// ```
        field: &'a [u8],
        /// Lower bound, inclusive.
        ///
        /// ```
        /// # use kevy_embedded::*;
        /// # let s = Store::open(Config::default())?;
        /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
        /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
        /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
        /// # for (k, age, status, total) in rows {
        /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
        /// # }
        /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
        /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
        /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
        /// # };
        /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
        /// let f = [ValueFilter::Range { field: b"total", min: b"8", max: b"99" }];
        /// assert_eq!(keys(ScalarQueryOpts::default().with_filters(&f))?.len(), 2, "8 itself is in");
        /// # Ok::<(), kevy_embedded::KevyError>(())
        /// ```
        min: &'a [u8],
        /// Upper bound, inclusive.
        ///
        /// ```
        /// # use kevy_embedded::*;
        /// # let s = Store::open(Config::default())?;
        /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
        /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
        /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
        /// # for (k, age, status, total) in rows {
        /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
        /// # }
        /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
        /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
        /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
        /// # };
        /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
        /// let f = [ValueFilter::Range { field: b"total", min: b"0", max: b"8" }];
        /// assert_eq!(keys(ScalarQueryOpts::default().with_filters(&f))?.len(), 2, "8 itself is in");
        /// # Ok::<(), kevy_embedded::KevyError>(())
        /// ```
        max: &'a [u8],
    },
    /// `field` exactly `value`.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let f = [ValueFilter::Eq { field: b"status", value: b"due" }];
    /// assert_eq!(keys(ScalarQueryOpts::default().with_filters(&f))?, [b"u:3".to_vec()]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    Eq {
        /// The declared value field to read.
        ///
        /// ```
        /// # use kevy_embedded::*;
        /// # let s = Store::open(Config::default())?;
        /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
        /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
        /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
        /// # for (k, age, status, total) in rows {
        /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
        /// # }
        /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
        /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
        /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
        /// # };
        /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
        /// // a field the index does not store is refused, not read as empty
        /// let f = [ValueFilter::Eq { field: b"email", value: b"x" }];
        /// assert!(keys(ScalarQueryOpts::default().with_filters(&f)).is_err());
        /// # Ok::<(), kevy_embedded::KevyError>(())
        /// ```
        field: &'a [u8],
        /// The value to match.
        ///
        /// ```
        /// # use kevy_embedded::*;
        /// # let s = Store::open(Config::default())?;
        /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
        /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
        /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
        /// # for (k, age, status, total) in rows {
        /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
        /// # }
        /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
        /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
        /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
        /// # };
        /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
        /// let f = [ValueFilter::Eq { field: b"total", value: b"20" }];
        /// assert_eq!(keys(ScalarQueryOpts::default().with_filters(&f))?, [b"u:2".to_vec()]);
        /// # Ok::<(), kevy_embedded::KevyError>(())
        /// ```
        value: &'a [u8],
    },
}

impl ValueFilter<'_> {
    pub(crate) fn field(&self) -> &[u8] {
        match self {
            ValueFilter::Range { field, .. } | ValueFilter::Eq { field, .. } => field,
        }
    }
}

/// Everything a scalar RANGE/EQ query carries beyond its bounds and
/// limit — the embedded twin of the wire's optional clauses.
/// [`ScalarQueryOpts::default`] is the plain query; the `with_*` methods
/// name the clauses a caller wants.
///
/// ```
/// use kevy_embedded::{ScalarQueryOpts, SortOrder, ValueFilter};
///
/// let filters = [ValueFilter::Eq { field: b"status", value: b"paid" }];
/// let opts = ScalarQueryOpts::default()
///     .with_filters(&filters)
///     .with_sort(b"total", SortOrder::Desc)
///     .with_offset(20);
/// assert_eq!(opts.sort, Some((&b"total"[..], SortOrder::Desc)));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ScalarQueryOpts<'a> {
    /// `FILTER …`: non-scoring predicates over stored values, ANDed. A
    /// row without the stored value fails — absent is not a value.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let paid = ValueFilter::Eq { field: b"status", value: b"paid" };
    /// let big = ValueFilter::Range { field: b"total", min: b"10", max: b"99" };
    /// let both = [paid, big];
    /// let opts = ScalarQueryOpts::default().with_filters(&both);
    /// assert_eq!(keys(opts)?, [b"u:2".to_vec()], "both predicates hold");
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub filters: &'a [ValueFilter<'a>],
    /// `SORT <field> ASC|DESC`: order the page by a stored value; a row
    /// with no usable value sorts last in both directions.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let opts = ScalarQueryOpts::default().with_sort(b"total", SortOrder::Desc);
    /// assert_eq!(keys(opts)?, [b"u:2".to_vec(), b"u:3".to_vec(), b"u:1".to_vec()]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub sort: Option<(&'a [u8], SortOrder)>,
    /// `DISTINCT <field>`: at most one row per coerced value; a row
    /// with no value is its own group.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let opts = ScalarQueryOpts::default().with_distinct(b"status");
    /// assert_eq!(keys(opts)?.len(), 2, "one paid row, one due row");
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub distinct: Option<&'a [u8]>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set (FILTER reduces the counts; DISTINCT does not).
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let fields = [b"status".to_vec()];
    /// let opts = ScalarQueryOpts::default().with_facets(&fields);
    /// let page = s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?;
    /// assert_eq!(page.facets[0], [(b"paid".to_vec(), 2), (b"due".to_vec(), 1)]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub facets: &'a [Vec<u8>],
    /// `OFFSET n`: rows to skip before `limit` takes effect.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let st = [(&b"status"[..], IndexValType::Str), (b"total", IndexValType::I64)];
    /// # s.idx_create_with_values(b"by_age", b"u:", b"age", IndexValType::I64, IndexKind::Range, &st)?;
    /// # let rows = [(&b"u:1"[..], &b"30"[..], &b"paid"[..], &b"5"[..]), (b"u:2", b"40", b"paid", b"20"), (b"u:3", b"50", b"due", b"8")];
    /// # for (k, age, status, total) in rows {
    /// #     s.hset(k, &[(b"age", age), (b"status", status), (b"total", total)])?;
    /// # }
    /// # let (lo, hi) = (IndexValue::I64(0), IndexValue::I64(99));
    /// # let keys = |opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     Ok(s.idx_query_claused(b"by_age", &lo, &hi, None, 10, opts)?.rows.into_iter().map(|r| r.0).collect())
    /// # };
    /// // u:1 (age 30, paid, total 5), u:2 (40, paid, 20), u:3 (50, due, 8)
    /// let opts = ScalarQueryOpts::default().with_offset(2);
    /// assert_eq!(keys(opts)?, [b"u:3".to_vec()]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub offset: usize,
}

impl<'a> ScalarQueryOpts<'a> {
    /// Set [`Self::filters`].
    ///
    /// ```
    /// let f = [kevy_embedded::ValueFilter::Eq { field: b"k", value: b"v" }];
    /// assert_eq!(kevy_embedded::ScalarQueryOpts::default().with_filters(&f).filters.len(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_filters(mut self, filters: &'a [ValueFilter<'a>]) -> Self {
        self.filters = filters;
        self
    }

    /// Set [`Self::sort`]: order by `field`, in `order`.
    ///
    /// ```
    /// use kevy_embedded::{ScalarQueryOpts, SortOrder};
    /// let o = ScalarQueryOpts::default().with_sort(b"ts", SortOrder::Asc);
    /// assert!(o.sort.is_some());
    /// ```
    #[inline]
    #[must_use]
    pub fn with_sort(mut self, field: &'a [u8], order: SortOrder) -> Self {
        self.sort = Some((field, order));
        self
    }

    /// Set [`Self::distinct`].
    ///
    /// ```
    /// let o = kevy_embedded::ScalarQueryOpts::default().with_distinct(b"user");
    /// assert_eq!(o.distinct, Some(&b"user"[..]));
    /// ```
    #[inline]
    #[must_use]
    pub fn with_distinct(mut self, field: &'a [u8]) -> Self {
        self.distinct = Some(field);
        self
    }

    /// Set [`Self::facets`].
    ///
    /// ```
    /// let fields = [b"status".to_vec()];
    /// let o = kevy_embedded::ScalarQueryOpts::default().with_facets(&fields);
    /// assert_eq!(o.facets.len(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_facets(mut self, facets: &'a [Vec<u8>]) -> Self {
        self.facets = facets;
        self
    }

    /// Set [`Self::offset`].
    ///
    /// ```
    /// assert_eq!(kevy_embedded::ScalarQueryOpts::default().with_offset(5).offset, 5);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Whether any clause reshapes the selection (the cursor-refusing
    /// set — `FILTER` alone pages fine).
    pub(crate) fn selects(&self) -> bool {
        self.sort.is_some() || self.distinct.is_some() || !self.facets.is_empty() || self.offset > 0
    }
}

#[cfg(feature = "text")]
pub use super::match_opts::MatchOpts;
