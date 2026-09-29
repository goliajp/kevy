//! What a query asks for, beyond its text and result limit — the clause
//! options and the shapes they answer in. A child module of `segment`
//! (declared via `#[path]`), re-exported from it, so the crate's public
//! surface is unchanged by the split.

use std::collections::HashMap;

/// One ranked hit.
///
/// ```
/// let m = kevy_text::TextMatch::new(b"doc:1".to_vec(), 1.5);
/// assert_eq!((m.key.as_slice(), m.score), (&b"doc:1"[..], 1.5));
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct TextMatch {
    /// Row key.
    pub key: Vec<u8>,
    /// Shard-local BM25 score.
    pub score: f64,
}

impl TextMatch {
    /// A hit for `key` at `score` — how a merge of several segments'
    /// pages, or of hot and cold hits, reports its union.
    ///
    /// ```
    /// let m = kevy_text::TextMatch::new(b"k".to_vec(), 0.25);
    /// assert_eq!(m.score, 0.25);
    /// ```
    #[must_use]
    pub fn new(key: Vec<u8>, score: f64) -> Self {
        Self { key, score }
    }
}

/// Corpus statistics supplied from outside a segment, for scoring one
/// shard's documents against the whole corpus rather than its own slice.
///
/// A cross-shard text query builds this by summing each shard's local
/// `n_docs` / `total_len` and, for each query token, its `df`. `df`
/// need only carry the query's tokens — the values a query actually
/// scores with — which is why global BM25 does not need a whole-corpus
/// df table.
///
/// ```
/// use std::collections::HashMap;
/// let stats = kevy_text::CorpusStats::new(10.0, 4.0, HashMap::from([(b"rust".to_vec(), 3)]));
/// assert_eq!(stats.df.get(&b"rust"[..]), Some(&3));
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct CorpusStats {
    /// Total documents across the corpus.
    pub n_docs: f64,
    /// Mean document length (unweighted tokens) across the corpus.
    pub avgdl: f64,
    /// Global document frequency per query token; a token missing here
    /// falls back to the segment's local list length.
    pub df: HashMap<Vec<u8>, u32>,
}

impl CorpusStats {
    /// Statistics for a corpus of `n_docs` documents of mean length
    /// `avgdl`, with the document frequency of each query token.
    ///
    /// ```
    /// let s = kevy_text::CorpusStats::new(2.0, 3.5, Default::default());
    /// assert_eq!((s.n_docs, s.avgdl), (2.0, 3.5));
    /// ```
    #[must_use]
    pub fn new(n_docs: f64, avgdl: f64, df: HashMap<Vec<u8>, u32>) -> Self {
        Self { n_docs, avgdl, df }
    }
}

/// A non-scoring predicate over a document's stored values.
///
/// The test takes raw bytes because this crate does not know what a
/// number or a date is; the caller coerces. A document with no value for
/// the field never passes — absent is not a value.
///
/// ```
/// let even = |v: &[u8]| v.last().is_some_and(|b| b % 2 == 0);
/// let f = kevy_text::Filter::new(0, &even);
/// assert!((f.test)(b"2") && !(f.test)(b"3"));
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Filter<'a> {
    /// Which declared value field the predicate reads.
    pub field: usize,
    /// The test applied to that field's bytes.
    pub test: &'a dyn Fn(&[u8]) -> bool,
}

impl<'a> Filter<'a> {
    /// A predicate `test` over declared value field `field`.
    ///
    /// ```
    /// let any = |_: &[u8]| true;
    /// assert_eq!(kevy_text::Filter::new(2, &any).field, 2);
    /// ```
    #[must_use]
    pub fn new(field: usize, test: &'a dyn Fn(&[u8]) -> bool) -> Self {
        Self { field, test }
    }
}

/// Which way an order runs: ascending or descending.
///
/// ```
/// use std::cmp::Ordering;
/// use kevy_text::SortOrder;
/// assert_eq!(SortOrder::Asc.apply(1.cmp(&2)), Ordering::Less);
/// assert_eq!(SortOrder::Desc.apply(1.cmp(&2)), Ordering::Greater);
/// assert_eq!(SortOrder::default(), SortOrder::Asc);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SortOrder {
    /// Smallest first.
    #[default]
    Asc,
    /// Largest first.
    Desc,
}

impl SortOrder {
    /// `ord` as this direction sees it: unchanged ascending, reversed
    /// descending.
    ///
    /// ```
    /// use kevy_text::SortOrder;
    /// let mut v = [3, 1, 2];
    /// v.sort_by(|a, b| SortOrder::Desc.apply(a.cmp(b)));
    /// assert_eq!(v, [3, 2, 1]);
    /// ```
    #[must_use]
    pub fn apply(self, ord: std::cmp::Ordering) -> std::cmp::Ordering {
        match self {
            Self::Asc => ord,
            Self::Desc => ord.reverse(),
        }
    }
}

/// An order to select the top hits by, other than the score.
///
/// The key function maps a stored value's raw bytes to an
/// order-preserving encoding, computed once per candidate; the segment
/// then compares bytes and never learns what a number is. `None` from it
/// means the document has no usable value for the field, which sorts
/// **last in both directions** — missing is not a value, and placing it
/// at one end or the other by direction would make "the oldest" and "the
/// newest" disagree about where the unknowns went.
///
/// ```
/// use kevy_text::{Sort, SortOrder};
/// let key = |v: &[u8]| Some(v.to_vec());
/// let s = Sort::new(0, &key).with_order(SortOrder::Desc);
/// assert_eq!((s.field, s.order), (0, SortOrder::Desc));
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Sort<'a> {
    /// Which declared value field orders the result.
    pub field: usize,
    /// Which way it orders.
    pub order: SortOrder,
    /// The order-preserving encoding of one stored value.
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

impl<'a> Sort<'a> {
    /// Ascending by field `field`, encoded by `key`.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(kevy_text::Sort::new(1, &key).order, kevy_text::SortOrder::Asc);
    /// ```
    #[must_use]
    pub fn new(field: usize, key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        Self { field, order: SortOrder::Asc, key }
    }

    /// The same sort, running `order`.
    ///
    /// ```
    /// use kevy_text::{Sort, SortOrder};
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(Sort::new(1, &key).with_order(SortOrder::Desc).order, SortOrder::Desc);
    /// ```
    #[must_use]
    pub fn with_order(mut self, order: SortOrder) -> Self {
        self.order = order;
        self
    }
}

/// Collapse the page so only the best document per value of a stored
/// field appears.
///
/// The key is the value's *identity*, coerced — so `1` and `1.0` in a
/// field declared `f64` are one value rather than two. A document with no
/// value for the field is its own group: `DISTINCT` removes documents
/// shown to share a value, and one that has none has not been shown to
/// share anything.
///
/// ```
/// let key = |v: &[u8]| Some(v.to_vec());
/// assert_eq!(kevy_text::Distinct::new(3, &key).field, 3);
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Distinct<'a> {
    /// Which declared value field identifies a group.
    pub field: usize,
    /// The identity of one stored value.
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

impl<'a> Distinct<'a> {
    /// One hit per identity `key` gives field `field`.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(kevy_text::Distinct::new(0, &key).field, 0);
    /// ```
    #[must_use]
    pub fn new(field: usize, key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        Self { field, key }
    }
}

/// Count the values of a stored field over the whole match set.
///
/// Buckets are keyed by the value's *identity* — the same coerced key
/// `DISTINCT` groups by, so `1` and `1.0` in a field declared `f64` are
/// one bucket — while the reported label is a spelling that really occurs
/// in the corpus rather than a re-serialisation.
///
/// ```
/// let key = |v: &[u8]| Some(v.to_ascii_lowercase());
/// assert_eq!(kevy_text::Facet::new(1, &key).field, 1);
/// ```
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Facet<'a> {
    /// Which declared value field to count.
    pub field: usize,
    /// The identity of one stored value.
    pub key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
}

impl<'a> Facet<'a> {
    /// Count field `field` by the identity `key` gives it.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// assert_eq!(kevy_text::Facet::new(4, &key).field, 4);
    /// ```
    #[must_use]
    pub fn new(field: usize, key: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>) -> Self {
        Self { field, key }
    }
}

/// One value bucket: the identity a cross-shard merge sums by, a spelling
/// of it that occurs in the corpus, and how many documents matched with
/// it.
pub type Bucket = (Vec<u8>, Vec<u8>, u64);

/// One faceted query's answer: the page, and a count per value for each
/// requested field.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct FacetedMatches {
    /// The ranked page, exactly what an unfaceted query would return.
    pub hits: Vec<TextMatch>,
    /// Per requested facet field, `(identity, label, count)` over the
    /// whole match set. The identity is what a cross-shard merge sums by;
    /// the label is what it reports.
    pub facets: Vec<Vec<Bucket>>,
}

/// Everything a MATCH query carries beyond its text and result limit.
///
/// Grouping them keeps the query entry point from growing a parameter per
/// clause, and gives every clause one place to be defaulted from
/// ([`QueryOpts::default`] is the plain, exact, unscoped query). Set what
/// differs with the `with_*` builders or by assigning the fields.
///
/// ```
/// let q = kevy_text::QueryOpts::default().with_typo(1).with_fields(&[0]);
/// assert_eq!((q.typo, q.fields), (1, &[0usize][..]));
/// ```
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct QueryOpts<'a> {
    /// Corpus-wide BM25 statistics — the second pass of a cross-shard
    /// query. `None` scores against this segment's own slice.
    pub stats: Option<&'a CorpusStats>,
    /// Edit distance allowed on bare terms (`TYPO n`); 0 = exact.
    pub typo: u32,
    /// Field positions the query is restricted to (`IN <field…>`); empty
    /// = every field.
    pub fields: &'a [usize],
    /// `FILTER`: non-scoring predicates, ANDed. Applied before the top-K
    /// — filtering afterwards would return fewer hits than exist.
    pub filter: &'a [Filter<'a>],
    /// `SORT`: select by a stored value instead of by score. Selecting,
    /// not re-ordering: a document that wins on the sort key must be
    /// chosen even when its score would never have reached the page.
    pub sort: Option<Sort<'a>>,
    /// `DISTINCT`: at most one hit per value of a stored field. Applied
    /// during selection, so the page is filled with `limit` DISTINCT
    /// documents rather than `limit` documents that then collapse.
    pub distinct: Option<Distinct<'a>>,
}

impl<'a> QueryOpts<'a> {
    /// Score against corpus-wide `stats` rather than this segment's slice.
    ///
    /// ```
    /// let s = kevy_text::CorpusStats::default();
    /// assert!(kevy_text::QueryOpts::default().with_stats(&s).stats.is_some());
    /// ```
    #[must_use]
    pub fn with_stats(mut self, stats: &'a CorpusStats) -> Self {
        self.stats = Some(stats);
        self
    }

    /// Allow `typo` edits on bare terms.
    ///
    /// ```
    /// assert_eq!(kevy_text::QueryOpts::default().with_typo(2).typo, 2);
    /// ```
    #[must_use]
    pub fn with_typo(mut self, typo: u32) -> Self {
        self.typo = typo;
        self
    }

    /// Restrict the query to these field positions.
    ///
    /// ```
    /// assert_eq!(kevy_text::QueryOpts::default().with_fields(&[1, 2]).fields, &[1, 2]);
    /// ```
    #[must_use]
    pub fn with_fields(mut self, fields: &'a [usize]) -> Self {
        self.fields = fields;
        self
    }

    /// Keep only documents every one of `filter` passes.
    ///
    /// ```
    /// let any = |_: &[u8]| true;
    /// let f = [kevy_text::Filter::new(0, &any)];
    /// assert_eq!(kevy_text::QueryOpts::default().with_filter(&f).filter.len(), 1);
    /// ```
    #[must_use]
    pub fn with_filter(mut self, filter: &'a [Filter<'a>]) -> Self {
        self.filter = filter;
        self
    }

    /// Select by a stored value instead of by score.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// let q = kevy_text::QueryOpts::default().with_sort(kevy_text::Sort::new(0, &key));
    /// assert!(q.sort.is_some());
    /// ```
    #[must_use]
    pub fn with_sort(mut self, sort: Sort<'a>) -> Self {
        self.sort = Some(sort);
        self
    }

    /// At most one hit per value of a stored field.
    ///
    /// ```
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// let q = kevy_text::QueryOpts::default().with_distinct(kevy_text::Distinct::new(0, &key));
    /// assert!(q.distinct.is_some());
    /// ```
    #[must_use]
    pub fn with_distinct(mut self, distinct: Distinct<'a>) -> Self {
        self.distinct = Some(distinct);
        self
    }
}

impl core::fmt::Debug for Filter<'_> {
    /// Prints every field except `test`.
    ///
    /// The predicate is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Filter").field("field", &self.field).field("test", &"<fn>").finish()
    }
}

impl core::fmt::Debug for Sort<'_> {
    /// Prints every field except `key`.
    ///
    /// The ordering key is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sort")
            .field("field", &self.field)
            .field("order", &self.order)
            .field("key", &"<fn>")
            .finish()
    }
}

impl core::fmt::Debug for Distinct<'_> {
    /// Prints every field except `key`.
    ///
    /// The identity key is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Distinct").field("field", &self.field).field("key", &"<fn>").finish()
    }
}

impl core::fmt::Debug for Facet<'_> {
    /// Prints every field except `key`.
    ///
    /// The bucketing key is a `&dyn Fn`, which has no `Debug` and no stable
    /// identity worth printing — it shows as `<fn>` so the rest of the
    /// struct stays inspectable.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Facet").field("field", &self.field).field("key", &"<fn>").finish()
    }
}
