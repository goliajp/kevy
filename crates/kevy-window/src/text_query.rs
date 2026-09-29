//! The cold directory's query face — what the two MATCH passes read
//! out of the frozen buckets. Pass 1 takes the live corpus counters;
//! pass 2 takes a whole clause-faithful page: bare terms and phrases
//! accumulate (the hot engine's exact clause semantics, over the
//! frozen postings), FILTER prunes on the frozen stored values,
//! FACET counts the filtered match set, and selection runs in the
//! page's own order — score order, or `sorted_order` under SORT, the
//! same rule the hot top-K and the cross-shard merge use.
//!
//! A child module of [`super`] (`#[path]`), so it reaches the cold
//! directory's private shape.

use std::collections::HashMap;

use kevy_text::cold::{decode_fwd, posting_df, score_cold, score_cold_phrase};
use kevy_text::{CorpusStats, SortOrder, sorted_order};

use super::{ColdHit, ColdPage, TextColdDir};

/// Everything pass 2 asks of the cold directory.
///
/// Built from the MATCH text with [`ColdPageQuery::parse`], then narrowed
/// by assigning the clause fields or with the `with_*` builders.
///
/// ```
/// let stats = kevy_text::CorpusStats::default();
/// let q = kevy_window::ColdPageQuery::parse(b"pear apple apple \"red fig\"", &stats, 10);
/// assert_eq!(q.bare, [b"apple".to_vec(), b"pear".to_vec()]);
/// assert_eq!(q.phrases.len(), 1);
/// assert!(q.filter.is_empty() && q.sort.is_none());
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ColdPageQuery<'a> {
    /// Bare terms, sorted and deduplicated (the hot engine's rule).
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let q = kevy_window::ColdPageQuery::parse(b"pear apple pear", &stats, 10);
    /// assert_eq!(q.bare, [b"apple".to_vec(), b"pear".to_vec()]);
    /// ```
    pub bare: Vec<Vec<u8>>,
    /// Each phrase's token sequence.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let q = kevy_window::ColdPageQuery::parse(b"\"red fig\" apple", &stats, 10);
    /// assert_eq!(q.phrases, [vec![b"red".to_vec(), b"fig".to_vec()]]);
    /// ```
    pub phrases: Vec<Vec<Vec<u8>>>,
    /// The injected global statistics both passes score with.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::new(1_000.0, 12.5, Default::default());
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10);
    /// assert_eq!(q.stats.n_docs, 1_000.0);
    /// ```
    pub stats: &'a CorpusStats,
    /// `FILTER` predicates, ANDed, over the frozen stored values.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let is_red = |v: &[u8]| v == b"red";
    /// let filter = [kevy_text::Filter::new(0, &is_red)];
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_filter(&filter);
    /// let page = cold.cold_page(&q);
    /// assert_eq!(page.hits.len(), 1);
    /// assert_eq!(page.hits[0].key, b"d:1");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub filter: &'a [kevy_text::Filter<'a>],
    /// `SORT`: the page order is the sort key's, not the score's.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let by_colour = |v: &[u8]| Some(v.to_vec());
    /// let sort = kevy_text::Sort::new(0, &by_colour);
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_sort(&sort);
    /// let keys: Vec<Vec<u8>> = cold.cold_page(&q).hits.into_iter().map(|h| h.key).collect();
    /// assert_eq!(keys, [b"d:2".to_vec(), b"d:1".to_vec()]); // "green" < "red"
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub sort: Option<&'a kevy_text::Sort<'a>>,
    /// `DISTINCT`: collapse to the best hit per value identity.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let colour = |v: &[u8]| Some(v.to_vec());
    /// let distinct = kevy_text::Distinct::new(0, &colour);
    /// let q = kevy_window::ColdPageQuery::parse(b"red", &stats, 10);
    /// assert_eq!(cold.cold_page(&q).hits.len(), 1); // only d:1 of the frozen two says "red"
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_distinct(&distinct);
    /// assert_eq!(cold.cold_page(&q).hits.len(), 2); // two colours, one hit each
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub distinct: Option<&'a kevy_text::Distinct<'a>>,
    /// `FACET` fields to count over the (filtered) match set.
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let colour = |v: &[u8]| Some(v.to_vec());
    /// let facets = [kevy_text::Facet::new(0, &colour)];
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 10).with_facets(&facets);
    /// assert_eq!(cold.cold_page(&q).facets[0].len(), 2); // red and green
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub facets: &'a [kevy_text::Facet<'a>],
    /// How deep a page the merge needs (LIMIT + OFFSET).
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let q = kevy_window::ColdPageQuery::parse(b"apple", &stats, 1);
    /// assert_eq!(q.fetch, 1);
    /// assert_eq!(cold.cold_page(&q).hits.len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fetch: usize,
}

impl<'a> ColdPageQuery<'a> {
    /// The query for MATCH `text`: bare terms sorted and deduplicated and
    /// phrases split the hot engine's way, scored against `stats`, `fetch`
    /// hits deep, with no FILTER, SORT, DISTINCT or FACET clause.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let q = kevy_window::ColdPageQuery::parse(b"open", &stats, 5);
    /// assert_eq!((q.bare.len(), q.fetch), (1, 5));
    /// ```
    pub fn parse(text: &[u8], stats: &'a CorpusStats, fetch: usize) -> Self {
        let (mut bare, phrases, _prefixes) = kevy_text::parse_clauses(text);
        bare.sort();
        bare.dedup();
        Self { bare, phrases, stats, filter: &[], sort: None, distinct: None, facets: &[], fetch }
    }

    /// Keep only documents every one of `filter` passes.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let any = |_: &[u8]| true;
    /// let f = [kevy_text::Filter::new(0, &any)];
    /// let q = kevy_window::ColdPageQuery::parse(b"open", &stats, 5).with_filter(&f);
    /// assert_eq!(q.filter.len(), 1);
    /// ```
    #[must_use]
    pub fn with_filter(mut self, filter: &'a [kevy_text::Filter<'a>]) -> Self {
        self.filter = filter;
        self
    }

    /// Order the page by a stored value instead of by score.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// let s = kevy_text::Sort::new(0, &key);
    /// let q = kevy_window::ColdPageQuery::parse(b"open", &stats, 5).with_sort(&s);
    /// assert!(q.sort.is_some());
    /// ```
    #[must_use]
    pub fn with_sort(mut self, sort: &'a kevy_text::Sort<'a>) -> Self {
        self.sort = Some(sort);
        self
    }

    /// Collapse to the best hit per value identity.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let key = |v: &[u8]| Some(v.to_vec());
    /// let d = kevy_text::Distinct::new(0, &key);
    /// let q = kevy_window::ColdPageQuery::parse(b"open", &stats, 5).with_distinct(&d);
    /// assert!(q.distinct.is_some());
    /// ```
    #[must_use]
    pub fn with_distinct(mut self, distinct: &'a kevy_text::Distinct<'a>) -> Self {
        self.distinct = Some(distinct);
        self
    }

    /// Count these facet fields over the (filtered) match set.
    ///
    /// ```
    /// let stats = kevy_text::CorpusStats::default();
    /// let label = |v: &[u8]| Some(v.to_vec());
    /// let f = [kevy_text::Facet::new(0, &label)];
    /// let q = kevy_window::ColdPageQuery::parse(b"open", &stats, 5).with_facets(&f);
    /// assert_eq!(q.facets.len(), 1);
    /// ```
    #[must_use]
    pub fn with_facets(mut self, facets: &'a [kevy_text::Facet<'a>]) -> Self {
        self.facets = facets;
        self
    }
}

impl TextColdDir {
    /// Pass-1 contribution: summed LIVE docs/length plus per-token
    /// live df across every cold segment (one fence descent per token
    /// per segment; the doc/length halves are in-memory numbers, no
    /// I/O at all).
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let (n_docs, total_len, df) = cold.cold_stats(&[b"apple".to_vec(), b"fig".to_vec()]);
    /// assert_eq!((n_docs, total_len), (2, 4)); // d:3 is still hot
    /// assert_eq!(df, [(b"apple".to_vec(), 2), (b"fig".to_vec(), 0)]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cold_stats(&self, tokens: &[Vec<u8>]) -> (u64, u64, Vec<(Vec<u8>, u32)>) {
        let n_docs: u64 = self.segs.iter().map(|c| c.n_docs).sum();
        let total_len: u64 = self.segs.iter().map(|c| c.total_len).sum();
        let df = tokens
            .iter()
            .map(|t| {
                let frozen: u32 = self
                    .segs
                    .iter()
                    .filter_map(|c| c.seg.get(t).ok().flatten())
                    .filter_map(|p| posting_df(&p))
                    .sum();
                let dead = self.df_dead.get(t).copied().unwrap_or(0);
                (t.clone(), frozen.saturating_sub(dead))
            })
            .collect();
        (n_docs, total_len, df)
    }

    /// Pass-2 contribution: the clause-faithful cold page (see the
    /// module doc for what each clause does here).
    ///
    /// ```
    /// # use kevy_text::{CorpusStats, SegmentShape, TextSegment};
    /// # let dir = kevy_tmpdir::TmpDir::new("text-cold-doc");
    /// let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    /// for (key, text, colour) in [("d:1", "red apple", "red"), ("d:2", "green apple", "green"), ("d:3", "red fig", "red")] {
    ///     ts.apply_doc(key.as_bytes(), Some(&[(text.as_bytes().to_vec(), 1.0)]), &[Some(colour.as_bytes())]);
    /// }
    /// let mut cold = kevy_window::TextColdDir::new();
    /// assert!(cold.freeze_batch(&mut ts, b"t.body", &[b"d:1".to_vec(), b"d:2".to_vec()], dir.path())?);
    /// let stats = CorpusStats::new(3.0, 2.0, Default::default());
    /// let page = cold.cold_page(&kevy_window::ColdPageQuery::parse(b"green apple", &stats, 10));
    /// assert_eq!(page.hits[0].key, b"d:2"); // matches both terms
    /// assert!(page.hits[0].score > page.hits[1].score);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn cold_page(&self, q: &ColdPageQuery) -> ColdPage {
        let acc = self.accumulate(q);
        let need_values = !q.filter.is_empty()
            || q.sort.is_some()
            || q.distinct.is_some()
            || !q.facets.is_empty();
        let mut values: HashMap<Vec<u8>, Vec<Option<Vec<u8>>>> = HashMap::new();
        let mut cands: Vec<ColdHit> = Vec::new();
        for (key, score) in acc {
            let vals = if need_values {
                let Some(v) = self.frozen_values(&key) else { continue };
                if !passes(&v, q.filter) {
                    continue;
                }
                Some(v)
            } else {
                None
            };
            let okey = q.sort.and_then(|s| vals.as_ref()?.get(s.field)?.as_deref().and_then(s.key));
            if let Some(v) = vals {
                values.insert(key.clone(), v);
            }
            cands.push(ColdHit { key, score, okey });
        }
        let facets = self.count_facets(q, &cands, &values);
        order_page(&mut cands, q.sort.map(|s| s.order));
        if let Some(d) = q.distinct {
            collapse(&mut cands, d, &values);
        }
        cands.truncate(q.fetch);
        values.retain(|k, _| cands.iter().any(|c| &c.key == k));
        ColdPage { hits: cands, values, facets }
    }

    /// Every clause's accumulated cold score, keyed by row key — the
    /// mirror of the hot `accumulate_clauses` over the frozen postings.
    fn accumulate(&self, q: &ColdPageQuery) -> HashMap<Vec<u8>, f64> {
        let mut acc = HashMap::new();
        for cs in &self.segs {
            let dead = |k: &[u8]| self.tombs.get(k).is_some_and(|s| s.contains(&cs.seq));
            for t in &q.bare {
                if let Ok(Some(payload)) = cs.seg.get(t) {
                    let _ = score_cold(&payload, t, q.stats, &dead, &mut acc);
                }
            }
            for phrase in &q.phrases {
                let payloads: Option<Vec<Vec<u8>>> =
                    phrase.iter().map(|t| cs.seg.get(t).ok().flatten()).collect();
                // A phrase token absent from this segment = the phrase
                // matches nothing here (the rarest-anchor None mirror).
                if let Some(payloads) = payloads {
                    let _ = score_cold_phrase(&payloads, phrase, q.stats, &dead, &mut acc);
                }
            }
        }
        acc
    }

    /// One live cold document's frozen stored values, from whichever
    /// segment holds its un-shadowed copy.
    fn frozen_values(&self, key: &[u8]) -> Option<Vec<Option<Vec<u8>>>> {
        let mut fwd_key = vec![0u8];
        fwd_key.extend_from_slice(key);
        for cs in &self.segs {
            if self.tombs.get(key).is_some_and(|s| s.contains(&cs.seq)) {
                continue;
            }
            if let Ok(Some(payload)) = cs.seg.get(&fwd_key) {
                return decode_fwd(&payload).map(|r| r.values);
            }
        }
        None
    }

    /// The cold half of each facet's count — the hot `count_facet`'s
    /// rules (filter applies, top-K and DISTINCT do not), ordered the
    /// same way; the shard merge sums it with the hot half.
    fn count_facets(
        &self,
        q: &ColdPageQuery,
        cands: &[ColdHit],
        values: &HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>,
    ) -> Vec<Vec<kevy_text::Bucket>> {
        q.facets
            .iter()
            .map(|f| {
                let mut counts: HashMap<Vec<u8>, (Vec<u8>, u64)> = HashMap::new();
                for c in cands {
                    let Some(raw) =
                        values.get(&c.key).and_then(|v| v.get(f.field)).and_then(Option::as_deref)
                    else {
                        continue;
                    };
                    let Some(k) = (f.key)(raw) else { continue };
                    counts.entry(k).or_insert_with(|| (raw.to_vec(), 0)).1 += 1;
                }
                let mut out: Vec<kevy_text::Bucket> =
                    counts.into_iter().map(|(k, (label, n))| (k, label, n)).collect();
                out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
                out
            })
            .collect()
    }
}

/// Order candidates by the page's rule: `sorted_order` under SORT
/// (a document WITH a value outranks one without, in both
/// directions), else score-descending with the row key as tiebreak.
fn order_page(cands: &mut [ColdHit], sort: Option<SortOrder>) {
    if let Some(order) = sort {
        cands.sort_by(|a, b| {
            sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), order)
        });
    } else {
        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.key.cmp(&b.key))
        });
    }
}

/// Collapse an ordered candidate list to the best hit per distinct
/// identity. A document with no value for the field is its own group
/// (the hot rule: it has not been shown to share anything).
fn collapse(
    cands: &mut Vec<ColdHit>,
    d: &kevy_text::Distinct,
    values: &HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>,
) {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    cands.retain(|c| {
        let identity = values
            .get(&c.key)
            .and_then(|v| v.get(d.field))
            .and_then(Option::as_deref)
            .and_then(d.key);
        match identity {
            None => true,
            Some(id) => seen.insert(id),
        }
    });
}

/// The hot `passes` mirror over frozen values: every predicate must
/// pass, and an absent value never does.
fn passes(values: &[Option<Vec<u8>>], filter: &[kevy_text::Filter]) -> bool {
    filter
        .iter()
        .all(|f| values.get(f.field).and_then(Option::as_deref).is_some_and(|v| (f.test)(v)))
}
