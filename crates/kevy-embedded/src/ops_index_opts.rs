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
    Range {
        /// The declared value field to read.
        field: &'a [u8],
        /// Lower bound, inclusive.
        min: &'a [u8],
        /// Upper bound, inclusive.
        max: &'a [u8],
    },
    /// `field` exactly `value`.
    Eq {
        /// The declared value field to read.
        field: &'a [u8],
        /// The value to match.
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
    pub filters: &'a [ValueFilter<'a>],
    /// `SORT <field> ASC|DESC`: order the page by a stored value; a row
    /// with no usable value sorts last in both directions.
    pub sort: Option<(&'a [u8], SortOrder)>,
    /// `DISTINCT <field>`: at most one row per coerced value; a row
    /// with no value is its own group.
    pub distinct: Option<&'a [u8]>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set (FILTER reduces the counts; DISTINCT does not).
    pub facets: &'a [Vec<u8>],
    /// `OFFSET n`: rows to skip before `limit` takes effect.
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

/// Everything a text MATCH carries beyond its index, query text and
/// result limit — the embedded twin of the wire's optional clauses.
///
/// Grouping them keeps one entry point instead of one per clause, and
/// [`MatchOpts::default`] is the plain query, so a caller opts into
/// exactly the clauses it names.
///
/// ```
/// use kevy_embedded::MatchOpts;
///
/// let every_field: &[Vec<u8>] = &[];
/// let opts = MatchOpts::default().with_highlight(every_field).with_typo(1);
/// assert_eq!(opts.typo, 1);
/// ```
#[cfg(feature = "text")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MatchOpts<'a> {
    /// `HIGHLIGHT`: `None` = not requested, `Some(&[])` = every indexed
    /// field, `Some(names)` = only those.
    pub highlight: Option<&'a [Vec<u8>]>,
    /// `TYPO n`: edit budget for each bare term; 0 = exact.
    pub typo: u32,
    /// `OFFSET n`: hits to skip before `limit` takes effect.
    pub offset: usize,
    /// `IN <field…>`: the declared field names to score within; empty =
    /// the whole document.
    pub scope: &'a [Vec<u8>],
    /// `FILTER …`: non-scoring predicates over stored values, ANDed.
    /// They decide which documents are eligible, not what a term is
    /// worth, so the corpus statistics stay whole-corpus.
    pub filters: &'a [ValueFilter<'a>],
    /// `SORT <field> ASC|DESC`: select by a stored value instead of by
    /// score. Selecting, not re-ordering — a document that wins on the
    /// key is chosen even when its score would never have reached the
    /// page.
    pub sort: Option<(&'a [u8], SortOrder)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field,
    /// applied during selection so the page holds `limit` distinct
    /// documents rather than `limit` that then collapse.
    pub distinct: Option<&'a [u8]>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set. Reported alongside the page rather than shaping it.
    pub facets: &'a [Vec<u8>],
}

#[cfg(feature = "text")]
impl<'a> MatchOpts<'a> {
    /// Set [`Self::highlight`] to these field names (`&[]` = every
    /// indexed field).
    ///
    /// ```
    /// let o = kevy_embedded::MatchOpts::default().with_highlight(&[]);
    /// assert!(o.highlight.is_some());
    /// ```
    #[inline]
    #[must_use]
    pub fn with_highlight(mut self, fields: &'a [Vec<u8>]) -> Self {
        self.highlight = Some(fields);
        self
    }

    /// Set [`Self::typo`].
    ///
    /// ```
    /// assert_eq!(kevy_embedded::MatchOpts::default().with_typo(2).typo, 2);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_typo(mut self, typo: u32) -> Self {
        self.typo = typo;
        self
    }

    /// Set [`Self::offset`].
    ///
    /// ```
    /// assert_eq!(kevy_embedded::MatchOpts::default().with_offset(10).offset, 10);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Set [`Self::scope`].
    ///
    /// ```
    /// let fields = [b"title".to_vec()];
    /// assert_eq!(kevy_embedded::MatchOpts::default().with_scope(&fields).scope.len(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_scope(mut self, scope: &'a [Vec<u8>]) -> Self {
        self.scope = scope;
        self
    }

    /// Set [`Self::filters`].
    ///
    /// ```
    /// let f = [kevy_embedded::ValueFilter::Eq { field: b"lang", value: b"en" }];
    /// assert_eq!(kevy_embedded::MatchOpts::default().with_filters(&f).filters.len(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_filters(mut self, filters: &'a [ValueFilter<'a>]) -> Self {
        self.filters = filters;
        self
    }

    /// Set [`Self::sort`]: select by `field`, in `order`.
    ///
    /// ```
    /// use kevy_embedded::{MatchOpts, SortOrder};
    /// assert!(MatchOpts::default().with_sort(b"ts", SortOrder::Desc).sort.is_some());
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
    /// let o = kevy_embedded::MatchOpts::default().with_distinct(b"author");
    /// assert_eq!(o.distinct, Some(&b"author"[..]));
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
    /// let fields = [b"lang".to_vec()];
    /// assert_eq!(kevy_embedded::MatchOpts::default().with_facets(&fields).facets.len(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_facets(mut self, facets: &'a [Vec<u8>]) -> Self {
        self.facets = facets;
        self
    }
}
