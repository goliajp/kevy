//! What a query asks for, beyond its text and result limit — the clause
//! options and the shapes they answer in. A child module of `segment`
//! (declared via `#[path]`), re-exported from it, so the crate's public
//! surface is unchanged by the split.

use std::collections::HashMap;

use super::{Distinct, Filter, Sort};

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
    ///
    /// ```
    /// let mut seg = kevy_text::TextSegment::new();
    /// seg.apply(b"user:7", Some(b"likes rust"));
    /// assert_eq!(seg.matches(b"rust", 10)[0].key, b"user:7");
    /// ```
    pub key: Vec<u8>,
    /// Shard-local BM25 score.
    ///
    /// Higher is better; the page comes back best first.
    ///
    /// ```
    /// let mut seg = kevy_text::TextSegment::new();
    /// seg.apply(b"short", Some(b"rust"));
    /// seg.apply(b"long", Some(b"rust in a much longer document"));
    /// let hits = seg.matches(b"rust", 10);
    /// assert_eq!(hits[0].key, b"short");
    /// assert!(hits[0].score > hits[1].score && hits[1].score > 0.0);
    /// ```
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
    ///
    /// One match among a thousand documents is rarer, so it scores higher,
    /// than the same match among two:
    ///
    /// ```
    /// use kevy_text::{CorpusStats, TextSegment};
    /// use std::collections::HashMap;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// let df = HashMap::from([(b"rust".to_vec(), 1)]);
    /// let small = CorpusStats::new(2.0, 2.0, df.clone());
    /// let large = CorpusStats::new(1000.0, 2.0, df);
    /// let score = |s| seg.matches_scored(b"rust", 1, Some(s))[0].score;
    /// assert!(score(&large) > score(&small));
    /// ```
    pub n_docs: f64,
    /// Mean document length (unweighted tokens) across the corpus.
    ///
    /// The same document is short against a corpus of long documents,
    /// and BM25 rewards a match in a short one:
    ///
    /// ```
    /// use kevy_text::{CorpusStats, TextSegment};
    /// use std::collections::HashMap;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// let df = HashMap::from([(b"rust".to_vec(), 1)]);
    /// let terse = CorpusStats::new(10.0, 2.0, df.clone());
    /// let verbose = CorpusStats::new(10.0, 50.0, df);
    /// let score = |s| seg.matches_scored(b"rust", 1, Some(s))[0].score;
    /// assert!(score(&verbose) > score(&terse));
    /// ```
    pub avgdl: f64,
    /// Global document frequency per query token; a token missing here
    /// falls back to the segment's local list length.
    ///
    /// ```
    /// use kevy_text::{CorpusStats, TextSegment};
    /// use std::collections::HashMap;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// let rare = CorpusStats::new(100.0, 2.0, HashMap::from([(b"rust".to_vec(), 1)]));
    /// let common = CorpusStats::new(100.0, 2.0, HashMap::from([(b"rust".to_vec(), 90)]));
    /// let score = |s| seg.matches_scored(b"rust", 1, Some(s))[0].score;
    /// assert!(score(&rare) > score(&common));
    /// // absent from `df`: this segment's own count (1) stands in
    /// let fallback = CorpusStats::new(100.0, 2.0, HashMap::new());
    /// assert_eq!(score(&fallback), score(&rare));
    /// ```
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
    ///
    /// ```
    /// # use kevy_text::{QueryOpts, SegmentShape, Sort, SortOrder, TextSegment};
    /// # let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// # for (k, text, price) in [("a", "red shoe", "30"), ("b", "red hat", "10"), ("c", "red cap", "20")] {
    /// #     seg.apply_doc(k.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(price.as_bytes())]);
    /// # }
    /// // a "red shoe" 30, b "red hat" 10, c "red cap" 20
    /// let price = |v: &[u8]| Some(v.to_vec());
    /// let cheapest = QueryOpts::default().with_sort(Sort::new(0, &price).with_order(SortOrder::Asc));
    /// let page = seg.matches_query_with(b"red", 2, cheapest);
    /// assert_eq!(page.iter().map(|m| &m.key[..]).collect::<Vec<_>>(), [&b"b"[..], b"c"]);
    /// ```
    #[default]
    Asc,
    /// Largest first.
    ///
    /// ```
    /// # use kevy_text::{QueryOpts, SegmentShape, Sort, SortOrder, TextSegment};
    /// # let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// # for (k, text, price) in [("a", "red shoe", "30"), ("b", "red hat", "10"), ("c", "red cap", "20")] {
    /// #     seg.apply_doc(k.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(price.as_bytes())]);
    /// # }
    /// // a "red shoe" 30, b "red hat" 10, c "red cap" 20
    /// let price = |v: &[u8]| Some(v.to_vec());
    /// let dearest = QueryOpts::default().with_sort(Sort::new(0, &price).with_order(SortOrder::Desc));
    /// let page = seg.matches_query_with(b"red", 2, dearest);
    /// assert_eq!(page.iter().map(|m| &m.key[..]).collect::<Vec<_>>(), [&b"a"[..], b"c"]);
    /// ```
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

/// One value bucket: the identity a cross-shard merge sums by, a spelling
/// of it that occurs in the corpus, and how many documents matched with
/// it.
///
/// ```
/// use kevy_text::{Bucket, Facet, QueryOpts, SegmentShape, TextSegment};
/// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
/// seg.apply_doc(b"a", Some(&[(b"red shoe".to_vec(), 1.0)]), &[Some(b"ACME")]);
/// seg.apply_doc(b"b", Some(&[(b"red hat".to_vec(), 1.0)]), &[Some(b"acme")]);
/// // identity folds case; the label is a spelling that really occurs
/// let brand = |v: &[u8]| Some(v.to_ascii_lowercase());
/// let r = seg.matches_query_faceted(b"red", 10, QueryOpts::default(), &[Facet::new(0, &brand)]);
/// let (identity, label, count): &Bucket = &r.facets[0][0];
/// assert_eq!((&identity[..], count), (&b"acme"[..], &2));
/// assert!(label == b"ACME" || label == b"acme");
/// ```
pub type Bucket = (Vec<u8>, Vec<u8>, u64);

/// One faceted query's answer: the page, and a count per value for each
/// requested field.
///
/// ```
/// use kevy_text::{Facet, QueryOpts, SegmentShape, TextSegment};
/// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
/// for (k, text, color) in [("a", "wool hat", "red"), ("b", "wool scarf", "red"), ("c", "wool sock", "blue")] {
///     seg.apply_doc(k.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(color.as_bytes())]);
/// }
/// let color = |v: &[u8]| Some(v.to_vec());
/// let r = seg.matches_query_faceted(b"wool", 1, QueryOpts::default(), &[Facet::new(0, &color)]);
/// // one hit on the page, but the counts cover all three matches
/// assert_eq!(r.hits.len(), 1);
/// assert_eq!(r.facets[0], [(b"red".to_vec(), b"red".to_vec(), 2), (b"blue".to_vec(), b"blue".to_vec(), 1)]);
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct FacetedMatches {
    /// The ranked page, exactly what an unfaceted query would return.
    ///
    /// ```
    /// use kevy_text::{Facet, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// seg.apply_doc(b"a", Some(&[(b"wool hat".to_vec(), 1.0)]), &[Some(b"red")]);
    /// seg.apply_doc(b"b", Some(&[(b"wool wool sock".to_vec(), 1.0)]), &[Some(b"blue")]);
    /// let color = |v: &[u8]| Some(v.to_vec());
    /// let r = seg.matches_query_faceted(b"wool", 10, QueryOpts::default(), &[Facet::new(0, &color)]);
    /// assert_eq!(r.hits, seg.matches_query_with(b"wool", 10, QueryOpts::default()));
    /// ```
    pub hits: Vec<TextMatch>,
    /// Per requested facet field, `(identity, label, count)` over the
    /// whole match set. The identity is what a cross-shard merge sums by;
    /// the label is what it reports.
    ///
    /// One list per requested facet, in request order, most frequent
    /// value first:
    ///
    /// ```
    /// use kevy_text::{Facet, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(2));
    /// seg.apply_doc(b"a", Some(&[(b"wool hat".to_vec(), 1.0)]), &[Some(b"red"), Some(b"S")]);
    /// seg.apply_doc(b"b", Some(&[(b"wool sock".to_vec(), 1.0)]), &[Some(b"blue"), Some(b"S")]);
    /// let raw = |v: &[u8]| Some(v.to_vec());
    /// let facets = [Facet::new(1, &raw), Facet::new(0, &raw)];
    /// let r = seg.matches_query_faceted(b"wool", 10, QueryOpts::default(), &facets);
    /// assert_eq!(r.facets.len(), 2);
    /// assert_eq!(r.facets[0], [(b"S".to_vec(), b"S".to_vec(), 2)]);
    /// assert_eq!(r.facets[1].len(), 2);
    /// ```
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
    ///
    /// ```
    /// use kevy_text::{CorpusStats, QueryOpts, TextSegment};
    /// use std::collections::HashMap;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// let corpus = CorpusStats::new(50.0, 4.0, HashMap::from([(b"rust".to_vec(), 5)]));
    /// let mut opts = QueryOpts::default();
    /// opts.stats = Some(&corpus);
    /// let global = seg.matches_query_with(b"rust", 1, opts)[0].score;
    /// assert_eq!(global, seg.matches_scored(b"rust", 1, Some(&corpus))[0].score);
    /// assert_ne!(global, seg.matches_query_with(b"rust", 1, QueryOpts::default())[0].score);
    /// ```
    pub stats: Option<&'a CorpusStats>,
    /// Edit distance allowed on bare terms (`TYPO n`); 0 = exact.
    ///
    /// ```
    /// use kevy_text::{QueryOpts, TextSegment};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"kevy storage"));
    /// assert!(seg.matches_query_with(b"storge", 10, QueryOpts::default()).is_empty());
    /// let mut fuzzy = QueryOpts::default();
    /// fuzzy.typo = 1;
    /// assert_eq!(seg.matches_query_with(b"storge", 10, fuzzy)[0].key, b"doc:1");
    /// ```
    pub typo: u32,
    /// Field positions the query is restricted to (`IN <field…>`); empty
    /// = every field.
    ///
    /// ```
    /// use kevy_text::{QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_fields(2));
    /// // field 0 = title, field 1 = body
    /// seg.apply_fields(b"doc:1", Some(&[(b"kevy".to_vec(), 1.0), (b"about redis".to_vec(), 1.0)]));
    /// assert_eq!(seg.matches_query_with(b"redis", 10, QueryOpts::default()).len(), 1);
    /// let mut titles = QueryOpts::default();
    /// titles.fields = &[0];
    /// assert!(seg.matches_query_with(b"redis", 10, titles).is_empty());
    /// ```
    pub fields: &'a [usize],
    /// `FILTER`: non-scoring predicates, ANDed. Applied before the top-K
    /// — filtering afterwards would return fewer hits than exist.
    ///
    /// ```
    /// # use kevy_text::{Filter, QueryOpts, SegmentShape, TextSegment};
    /// # let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// # for (k, text, price) in [("a", "red shoe", "30"), ("b", "red hat", "10"), ("c", "red cap", "20")] {
    /// #     seg.apply_doc(k.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(price.as_bytes())]);
    /// # }
    /// // a "red shoe" 30, b "red hat" 10, c "red cap" 20
    /// let under_25 = |v: &[u8]| v < &b"25"[..];
    /// let filter = [Filter::new(0, &under_25)];
    /// let mut opts = QueryOpts::default();
    /// opts.filter = &filter;
    /// let mut keys: Vec<_> = seg.matches_query_with(b"red", 10, opts).into_iter().map(|m| m.key).collect();
    /// keys.sort();
    /// assert_eq!(keys, [b"b".to_vec(), b"c".to_vec()]);
    /// ```
    pub filter: &'a [Filter<'a>],
    /// `SORT`: select by a stored value instead of by score. Selecting,
    /// not re-ordering: a document that wins on the sort key must be
    /// chosen even when its score would never have reached the page.
    ///
    /// ```
    /// use kevy_text::{QueryOpts, SegmentShape, Sort, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// seg.apply_doc(b"best", Some(&[(b"red red red".to_vec(), 1.0)]), &[Some(b"2024")]);
    /// seg.apply_doc(b"oldest", Some(&[(b"red and many other words".to_vec(), 1.0)]), &[Some(b"1999")]);
    /// assert_eq!(seg.matches_query_with(b"red", 1, QueryOpts::default())[0].key, b"best");
    /// let year = |v: &[u8]| Some(v.to_vec());
    /// let mut opts = QueryOpts::default();
    /// opts.sort = Some(Sort::new(0, &year));
    /// assert_eq!(seg.matches_query_with(b"red", 1, opts)[0].key, b"oldest");
    /// ```
    pub sort: Option<Sort<'a>>,
    /// `DISTINCT`: at most one hit per value of a stored field. Applied
    /// during selection, so the page is filled with `limit` DISTINCT
    /// documents rather than `limit` documents that then collapse.
    ///
    /// ```
    /// use kevy_text::{Distinct, QueryOpts, SegmentShape, TextSegment};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// seg.apply_doc(b"a1", Some(&[(b"red red".to_vec(), 1.0)]), &[Some(b"acme")]);
    /// seg.apply_doc(b"a2", Some(&[(b"red shoe".to_vec(), 1.0)]), &[Some(b"acme")]);
    /// seg.apply_doc(b"z1", Some(&[(b"red hat and scarf".to_vec(), 1.0)]), &[Some(b"zeta")]);
    /// let brand = |v: &[u8]| Some(v.to_vec());
    /// let mut opts = QueryOpts::default();
    /// opts.distinct = Some(Distinct::new(0, &brand));
    /// // two slots, filled by two different brands
    /// let page = seg.matches_query_with(b"red", 2, opts);
    /// assert_eq!(page.iter().map(|m| &m.key[..]).collect::<Vec<_>>(), [&b"a1"[..], b"z1"]);
    /// ```
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
