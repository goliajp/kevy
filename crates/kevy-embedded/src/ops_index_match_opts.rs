//! The text MATCH clause set, [`MatchOpts`]. Split from
//! `ops_index_opts.rs` for the 500-LOC house rule; a `#[path]` child of
//! `ops_index.rs`.

use kevy_index::SortOrder;

use super::opts::ValueFilter;

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MatchOpts<'a> {
    /// `HIGHLIGHT`: `None` = not requested, `Some(&[])` = every indexed
    /// field, `Some(names)` = only those.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let hl = MatchOpts::default().with_highlight(&[]);
    /// let hits = s.idx_match_with(b"ft", b"engine", 10, hl)?;
    /// // d:1 matched in `body`, bytes 7..13 ("engine")
    /// assert_eq!(hits[0].2, [(b"body".to_vec(), vec![(7, 13)])]);
    /// assert!(s.idx_match_with(b"ft", b"engine", 10, MatchOpts::default())?[0].2.is_empty());
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub highlight: Option<&'a [Vec<u8>]>,
    /// `TYPO n`: edit budget for each bare term; 0 = exact.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// assert!(keys(b"rusr", MatchOpts::default())?.is_empty());
    /// let one_edit = MatchOpts::default().with_typo(1);
    /// assert_eq!(keys(b"rusr", one_edit)?.len(), 3);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub typo: u32,
    /// `OFFSET n`: hits to skip before `limit` takes effect.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let all = s.idx_match_with(b"ft", b"rust", 10, MatchOpts::default())?;
    /// let skip = MatchOpts::default().with_offset(1);
    /// let rest = s.idx_match_with(b"ft", b"rust", 10, skip)?;
    /// assert_eq!(rest[0].0, all[1].0);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub offset: usize,
    /// `IN <field…>`: the declared field names to score within; empty =
    /// the whole document.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let title = [b"title".to_vec()];
    /// let opts = MatchOpts::default().with_scope(&title);
    /// assert_eq!(keys(b"rust", opts)?, [b"d:1".to_vec(), b"d:3".to_vec()], "d:2 has it only in body");
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub scope: &'a [Vec<u8>],
    /// `FILTER …`: non-scoring predicates over stored values, ANDed.
    /// They decide which documents are eligible, not what a term is
    /// worth, so the corpus statistics stay whole-corpus.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let en = [ValueFilter::Eq { field: b"lang", value: b"en" }];
    /// let opts = MatchOpts::default().with_filters(&en);
    /// assert_eq!(keys(b"rust", opts)?, [b"d:1".to_vec(), b"d:2".to_vec()]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub filters: &'a [ValueFilter<'a>],
    /// `SORT <field> ASC|DESC`: select by a stored value instead of by
    /// score. Selecting, not re-ordering — a document that wins on the
    /// key is chosen even when its score would never have reached the
    /// page.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let newest = MatchOpts::default().with_sort(b"ts", SortOrder::Desc);
    /// let hits = s.idx_match_with(b"ft", b"rust", 2, newest)?;
    /// assert_eq!((&hits[0].0[..], &hits[1].0[..]), (&b"d:3"[..], &b"d:2"[..]));
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub sort: Option<(&'a [u8], SortOrder)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field,
    /// applied during selection so the page holds `limit` distinct
    /// documents rather than `limit` that then collapse.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let per_lang = MatchOpts::default().with_distinct(b"lang");
    /// assert_eq!(s.idx_match_with(b"ft", b"rust", 10, per_lang)?.len(), 2, "one en hit, one ja hit");
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub distinct: Option<&'a [u8]>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set. Reported alongside the page rather than shaping it.
    ///
    /// ```
    /// # use kevy_embedded::*;
    /// # let s = Store::open(Config::default())?;
    /// # let fields = [(&b"title"[..], 1.0), (b"body", 1.0)];
    /// # let vals = [(&b"lang"[..], IndexValType::Str), (b"ts", IndexValType::I64)];
    /// # s.idx_create_text(b"ft", b"d:", &fields, TokenPositions::Omit, &vals)?;
    /// # for (k, title, body, lang, ts) in [
    /// #     (&b"d:1"[..], &b"rust"[..], &b"a fast engine"[..], &b"en"[..], &b"1"[..]),
    /// #     (b"d:2", b"engines", b"written in rust", b"en", b"2"),
    /// #     (b"d:3", b"rust notes", b"ja docs", b"ja", b"3"),
    /// # ] {
    /// #     s.hset(k, &[(b"title", title), (b"body", body), (b"lang", lang), (b"ts", ts)])?;
    /// # }
    /// # let keys = |q: &[u8], opts| -> KevyResult<Vec<Vec<u8>>> {
    /// #     let mut k: Vec<_> = s.idx_match_with(b"ft", q, 10, opts)?.into_iter().map(|h| h.0).collect();
    /// #     k.sort();
    /// #     Ok(k)
    /// # };
    /// // d:1 title "rust" (en, ts 1), d:2 body "written in rust" (en, 2), d:3 title "rust notes" (ja, 3)
    /// let lang = [b"lang".to_vec()];
    /// let opts = MatchOpts::default().with_facets(&lang);
    /// let page = s.idx_match_faceted(b"ft", b"rust", 1, opts)?;
    /// assert_eq!(page.hits.len(), 1);
    /// assert_eq!(page.facets[0], [(b"en".to_vec(), 2), (b"ja".to_vec(), 1)]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub facets: &'a [Vec<u8>],
}

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
