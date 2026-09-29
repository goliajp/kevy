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

use super::TextColdDir;

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
    pub bare: Vec<Vec<u8>>,
    /// Each phrase's token sequence.
    pub phrases: Vec<Vec<Vec<u8>>>,
    /// The injected global statistics both passes score with.
    pub stats: &'a CorpusStats,
    /// `FILTER` predicates, ANDed, over the frozen stored values.
    pub filter: &'a [kevy_text::Filter<'a>],
    /// `SORT`: the page order is the sort key's, not the score's.
    pub sort: Option<&'a kevy_text::Sort<'a>>,
    /// `DISTINCT`: collapse to the best hit per value identity.
    pub distinct: Option<&'a kevy_text::Distinct<'a>>,
    /// `FACET` fields to count over the (filtered) match set.
    pub facets: &'a [kevy_text::Facet<'a>],
    /// How deep a page the merge needs (LIMIT + OFFSET).
    pub fetch: usize,
}

/// One cold hit: its page-order ingredients, ready to merge.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ColdHit {
    /// The row key this hit points at.
    pub key: Vec<u8>,
    /// Its BM25 relevance. Comparable across segments because the
    /// document-frequency corrections are applied before the merge, not
    /// after — a per-segment score would not be.
    pub score: f64,
    /// The sort key, when the query sorts by a stored value.
    pub okey: Option<Vec<u8>>,
}

/// The cold half of one shard's pass-2 answer.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ColdPage {
    /// Best `fetch` cold hits in the page's order.
    pub hits: Vec<ColdHit>,
    /// The returned hits' frozen stored values — what the merge reads
    /// for sort/distinct identities and the origin's okeys/dkeys.
    pub values: HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>,
    /// Per requested facet field, (identity, label, count) over the
    /// filtered cold match set.
    pub facets: Vec<Vec<kevy_text::Bucket>>,
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
