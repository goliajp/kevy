//! Clause-carrying MATCH for the embedded API, split from
//! `ops_index.rs` for the 500-LOC house rule. A `#[path]` child module,
//! so it reaches `Store`'s crate-private index methods and fields.

use kevy_index::IndexSpec;

use super::claused::{ValueFilter, unknown_field, value_test};
use super::{FieldSpans, HighlightedHit, sync_segs};
use crate::store::{Store, lock_write};
use crate::{KevyError, KevyResult};

/// Everything a text MATCH carries beyond its index, query text and
/// result limit — the embedded twin of the wire's optional clauses.
///
/// Grouping them keeps one entry point instead of one per clause, and
/// [`MatchOpts::default`] is the plain query, so a caller opts into
/// exactly the clauses it names.
#[derive(Clone, Copy, Default)]
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
    pub sort: Option<(&'a [u8], bool)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field,
    /// applied during selection so the page holds `limit` distinct
    /// documents rather than `limit` that then collapse.
    pub distinct: Option<&'a [u8]>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set. Reported alongside the page rather than shaping it.
    pub facets: &'a [Vec<u8>],
}

impl Store {
    /// [`Self::idx_match`] with every optional clause: highlight spans,
    /// a typo budget, an offset, and a field scope.
    ///
    /// A scoped query is a field-scoped BM25 — frequency, length and
    /// document frequency all come from the named fields alone — so
    /// naming a field the index does not declare is an error rather than
    /// an empty result that would look like a working query.
    pub fn idx_match_with(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
        opts: MatchOpts<'_>,
    ) -> KevyResult<Vec<HighlightedHit>> {
        self.idx_match_faceted(name, query, limit, opts).map(|p| p.hits)
    }

    // The public entry is [`Store::idx_match_faceted`] in the advise
    // module, which wraps this run with the refusal/usage observation.
    pub(crate) fn match_faceted_run(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
        opts: MatchOpts<'_>,
    ) -> KevyResult<MatchPage> {
        let (limit, offset) = (limit.clamp(1, 1000), opts.offset.min(10_000));
        // Deep enough to skip OFFSET and still fill LIMIT post-merge.
        let fetch = limit + offset;
        super::text_cold::cold_refusal(self.text_has_cold(name), query, opts.typo, opts.scope)?;
        let (scope, tests) = self.resolve_clauses(name, opts.scope, opts.filters)?;
        let sorted = self.sort_field(name, opts.sort)?;
        let fkeys = self.facet_keys(name, opts.facets)?;
        let fac: Vec<kevy_text::Facet> = fkeys
            .iter()
            .map(|(field, k)| kevy_text::Facet { field: *field, key: k.as_ref() })
            .collect();
        let grouped = self.value_field("DISTINCT", name, opts.distinct)?;
        let dkey = grouped.map(|(_, ty)| move |raw: &[u8]| kevy_index::order_key(ty, raw));
        let distinct =
            grouped.zip(dkey.as_ref()).map(|((field, _), k)| kevy_text::Distinct { field, key: k });
        let key = sorted.map(|(_, _, ty)| move |raw: &[u8]| kevy_index::order_key(ty, raw));
        let sort = sorted.zip(key.as_ref()).map(|((field, desc, _), k)| kevy_text::Sort {
            field,
            desc,
            key: k,
        });
        let boxed = box_tests(tests);
        let filter: Vec<kevy_text::Filter> =
            boxed.iter().map(|(f, t)| kevy_text::Filter { field: *f, test: t.as_ref() }).collect();
        let stats = self.text_corpus_stats_in(name, query, opts.typo, &scope)?;
        let q = kevy_text::QueryOpts {
            stats: Some(&stats),
            typo: opts.typo,
            fields: &scope,
            filter: &filter,
            sort,
            distinct,
        };
        let (mut all, facets, cold_vals) =
            self.gather_hits(name, query, fetch, q, &stats, opts.highlight, &fac);
        self.order_page(name, &mut all, sorted, &cold_vals);
        self.collapse_union(name, &mut all, grouped, &cold_vals);
        if offset > 0 {
            all.drain(..offset.min(all.len()));
        }
        all.truncate(limit);
        Ok(MatchPage { hits: all, facets })
    }

    /// Resolve the clauses that need the index spec: `IN` names onto
    /// field positions, `FILTER` predicates onto stored-value positions
    /// and typed tests.
    ///
    /// One catalog read for both, and both fail loudly on a name the
    /// index does not offer — an unknown field could just as easily match
    /// nothing, but then a typo would return a result indistinguishable
    /// from a working query with no hits.
    fn resolve_clauses(
        &self,
        name: &[u8],
        scope: &[Vec<u8>],
        filters: &[ValueFilter<'_>],
    ) -> KevyResult<ResolvedClauses> {
        if scope.is_empty() && filters.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let guard = self.indexes.catalog.read().unwrap_or_else(|e| e.into_inner());
        let Some((spec, _)) = guard.1.get(name) else {
            return Err(KevyError::NotFound("no such text index".into()));
        };
        let mut positions = Vec::with_capacity(scope.len());
        for want in scope {
            let names = || spec.fields.iter().map(|f| f.name.as_slice()).collect::<Vec<_>>();
            let i = spec
                .fields
                .iter()
                .position(|f| f.name == *want)
                .ok_or_else(|| unknown_field("IN", want, "index", &names()))?;
            positions.push(i);
        }
        let tests = filters.iter().map(|f| value_test(spec, f)).collect::<KevyResult<Vec<_>>>()?;
        Ok((positions, tests))
    }
}

/// What the spec-dependent clauses resolve to: `IN`'s field positions,
/// and `FILTER`'s (stored-value position, typed test) pairs.
type ResolvedClauses = (Vec<usize>, Vec<(usize, kevy_index::ValueTest)>);

/// Each resolved test boxed as the closure the segment takes. The boxes
/// must outlive the borrowed `Filter` list, so they are returned rather
/// than built inline.
type ValuePred = Box<dyn Fn(&[u8]) -> bool>;

fn box_tests(tests: Vec<(usize, kevy_index::ValueTest)>) -> Vec<(usize, ValuePred)> {
    tests
        .into_iter()
        .map(|(f, t)| {
            let b: ValuePred = Box::new(move |v: &[u8]| t.passes(v));
            (f, b)
        })
        .collect()
}

impl Store {
    /// Every shard's page for this query, unmerged, with the facet
    /// buckets summed by identity as the shards report them.
    #[allow(clippy::too_many_arguments)]
    fn gather_hits(
        &self,
        name: &[u8],
        query: &[u8],
        fetch: usize,
        q: kevy_text::QueryOpts<'_>,
        stats: &kevy_text::CorpusStats,
        highlight: Option<&[Vec<u8>]>,
        facets: &[kevy_text::Facet],
    ) -> (Vec<HighlightedHit>, Vec<FacetCounts>, super::text_cold::ColdVals) {
        let mut all = Vec::new();
        let mut buckets: Vec<Vec<RawBucket>> = vec![Vec::new(); facets.len()];
        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let mut cold_vals = super::text_cold::ColdVals::new();
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((spec, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                // `matches_query_with` parses quoted phrases out of the
                // raw query text; with none it is the ordinary term query.
                let r = ts.matches_query_faceted(query, fetch, q, facets);
                absorb_live(r, ts, spec, query, highlight, &mut all, &mut buckets);
            }
            // The shard's frozen buckets join the union like one more
            // ranked contributor — the origin merge below re-orders,
            // re-collapses and truncates the lot.
            #[cfg(not(target_arch = "wasm32"))]
            gather_cold(
                inner,
                name,
                query,
                fetch,
                stats,
                &q,
                facets,
                highlight,
                &mut all,
                &mut buckets,
                &mut cold_vals,
            );
            #[cfg(target_arch = "wasm32")]
            let _ = stats;
        }
        (all, finish_buckets(buckets), cold_vals)
    }

    /// Put the merged hits in the page's order: by the sort key when the
    /// query gave one — the same definition each shard selected by — else
    /// by score.
    fn order_page(
        &self,
        name: &[u8],
        all: &mut Vec<HighlightedHit>,
        sorted: Option<(usize, bool, kevy_index::ValType)>,
        cold_vals: &super::text_cold::ColdVals,
    ) {
        let Some((field, desc, ty)) = sorted else {
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            return;
        };
        let mut keyed: Vec<(Option<Vec<u8>>, HighlightedHit)> = std::mem::take(all)
            .into_iter()
            .map(|h| (self.stored_order_key(name, &h.0, field, ty, cold_vals), h))
            .collect();
        keyed.sort_by(|a, b| {
            kevy_text::sorted_order((a.0.as_deref(), &a.1.0), (b.0.as_deref(), &b.1.0), desc)
        });
        *all = keyed.into_iter().map(|(_, h)| h).collect();
    }

    /// Collapse the union of the shards' pages: two shards can each hold
    /// a document with the same value, and only the better survives.
    ///
    /// `all` is already in the page's order, so the first occurrence of a
    /// value is its best and a stable retain keeps exactly that.
    /// Documents with no value are their own group and all survive — the
    /// same rule each shard collapsed by.
    fn collapse_union(
        &self,
        name: &[u8],
        all: &mut Vec<HighlightedHit>,
        grouped: Option<(usize, kevy_index::ValType)>,
        cold_vals: &super::text_cold::ColdVals,
    ) {
        let Some((field, ty)) = grouped else { return };
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        all.retain(|h| match self.stored_order_key(name, &h.0, field, ty, cold_vals) {
            Some(k) => seen.insert(k),
            None => true,
        });
    }

    /// Resolve a `SORT <field> ASC|DESC` clause to the stored-value
    /// position, the direction, and that field's declared type.
    fn sort_field(
        &self,
        name: &[u8],
        sort: Option<(&[u8], bool)>,
    ) -> KevyResult<Option<(usize, bool, kevy_index::ValType)>> {
        let Some((field, desc)) = sort else { return Ok(None) };
        let Some((pos, ty)) = self.value_field("SORT", name, Some(field))? else {
            return Ok(None);
        };
        Ok(Some((pos, desc, ty)))
    }

    /// Each `FACET` field's position paired with the order-preserving
    /// encoding of its declared type — the identity buckets are grouped
    /// by. Returned rather than built inline because the borrowed
    /// `Facet` list points at these closures.
    fn facet_keys(&self, name: &[u8], facets: &[Vec<u8>]) -> KevyResult<Vec<FacetKey>> {
        Ok(self
            .facet_fields(name, facets)?
            .into_iter()
            .map(|(field, ty)| -> FacetKey {
                (field, Box::new(move |raw: &[u8]| kevy_index::order_key(ty, raw)))
            })
            .collect())
    }

    /// Each `FACET` field's stored-value position and declared type.
    fn facet_fields(
        &self,
        name: &[u8],
        facets: &[Vec<u8>],
    ) -> KevyResult<Vec<(usize, kevy_index::ValType)>> {
        facets
            .iter()
            .map(|f| {
                Ok(self
                    .value_field("FACET", name, Some(f))?
                    .expect("a named field always resolves or errors"))
            })
            .collect()
    }

    /// A clause's named stored-value field, as a position and its
    /// declared type. Errors naming what the index does store.
    fn value_field(
        &self,
        clause: &str,
        name: &[u8],
        field: Option<&[u8]>,
    ) -> KevyResult<Option<(usize, kevy_index::ValType)>> {
        let Some(field) = field else { return Ok(None) };
        let guard = self.indexes.catalog.read().unwrap_or_else(|e| e.into_inner());
        let Some((spec, _)) = guard.1.get(name) else {
            return Err(KevyError::NotFound("no such text index".into()));
        };
        let stored: Vec<&[u8]> = spec.values.iter().map(|v| v.name.as_slice()).collect();
        let pos = spec
            .values
            .iter()
            .position(|v| v.name == field)
            .ok_or_else(|| unknown_field(clause, field, "store", &stored))?;
        Ok(Some((pos, spec.values[pos].ty)))
    }

    /// One row's sort key: its stored value, in the order-preserving
    /// encoding of that field's declared type.
    /// One row's sort key: its stored value from a hot segment, or —
    /// for a cold hit — from its frozen doc record.
    fn stored_order_key(
        &self,
        name: &[u8],
        key: &[u8],
        field: usize,
        ty: kevy_index::ValType,
        cold_vals: &super::text_cold::ColdVals,
    ) -> Option<Vec<u8>> {
        for shard in self.shards.iter() {
            let g = lock_write(shard);
            if let Some((_, ts)) = g.idx_segs.text.iter().find(|(s, _)| s.name == name)
                && let Some(raw) = ts.stored_value(key, field)
            {
                return kevy_index::order_key(ty, raw);
            }
        }
        let raw = cold_vals.get(key)?.get(field)?.as_deref()?;
        kevy_index::order_key(ty, raw)
    }
}

/// One shard's cold contribution to the union: page hits (each with
/// its row-read highlight when asked), facet buckets folded by
/// identity, and the frozen values the union's sort/distinct keys
/// will need.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
fn gather_cold(
    inner: &mut crate::store_inner::Inner,
    name: &[u8],
    query: &[u8],
    fetch: usize,
    stats: &kevy_text::CorpusStats,
    q: &kevy_text::QueryOpts<'_>,
    facets: &[kevy_text::Facet],
    highlight: Option<&[Vec<u8>]>,
    all: &mut Vec<HighlightedHit>,
    buckets: &mut [Vec<RawBucket>],
    cold_vals: &mut super::text_cold::ColdVals,
) {
    let Some(dir) = inner.idx_segs.cold_text_of(name).filter(|d| d.has_cold()) else {
        return;
    };
    let (mut bare, phrases, _prefixes) = kevy_text::parse_clauses(query);
    bare.sort();
    bare.dedup();
    let page = dir.cold_page(&kevy_window::ColdPageQuery {
        bare,
        phrases,
        stats,
        filter: q.filter,
        sort: q.sort.as_ref(),
        distinct: q.distinct.as_ref(),
        facets,
        fetch,
    });
    let spec = inner.idx_segs.text.iter().find(|(s, _)| s.name == name).map(|(s, _)| s.clone());
    for h in page.hits {
        let hl = highlight.map_or_else(Vec::new, |w| {
            spec.as_ref().map_or_else(Vec::new, |sp| {
                super::text_cold::cold_hit_highlight(&mut inner.store, sp, &h.key, query, w)
            })
        });
        all.push((h.key, h.score, hl));
    }
    for (into, from) in buckets.iter_mut().zip(page.facets) {
        fold_raw(into, from);
    }
    cold_vals.extend(page.values);
}

/// Fold one contributor's facet buckets into the running totals by
/// identity (two spellings of one value sum; the first label wins).
fn fold_raw(into: &mut Vec<RawBucket>, from: Vec<RawBucket>) {
    for (key, label, n) in from {
        match into.iter_mut().find(|(k, _, _)| *k == key) {
            Some(e) => e.2 += n,
            None => into.push((key, label, n)),
        }
    }
}

/// A facet field's position paired with the order-preserving encoding of
/// its declared type — the identity its buckets are grouped by.
type FacetKey = (usize, Box<dyn Fn(&[u8]) -> Option<Vec<u8>>>);

/// One facet field's reported buckets: `(value, count)`, most frequent
/// first.
pub type FacetCounts = Vec<(Vec<u8>, u64)>;

/// One facet bucket in flight, before the grouping identity is dropped.
type RawBucket = (Vec<u8>, Vec<u8>, u64);

/// A faceted query's answer: the page, and per requested `FACET` field
/// its `(value, count)` buckets over the whole match set.
#[derive(Debug)]
pub struct MatchPage {
    /// The ranked page — exactly what [`Store::idx_match_with`] returns.
    pub hits: Vec<HighlightedHit>,
    /// One entry per requested facet field, most frequent first.
    pub facets: Vec<FacetCounts>,
}

/// Drop the grouping identity and order the buckets for reporting: most
/// frequent first, the label breaking ties so the order is stable.
fn finish_buckets(buckets: Vec<Vec<RawBucket>>) -> Vec<FacetCounts> {
    buckets
        .into_iter()
        .map(|mut field| {
            field.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
            field.into_iter().map(|(_, label, n)| (label, n)).collect()
        })
        .collect()
}

/// Fold one live segment's answer into the running union: each hit with its
/// highlight spans, each facet's raw buckets into the matching accumulator.
///
/// Split from `gather_hits` for the 50-line rule.
fn absorb_live(
    r: kevy_text::FacetedMatches,
    ts: &kevy_text::TextSegment,
    spec: &IndexSpec,
    query: &[u8],
    highlight: Option<&[Vec<u8>]>,
    all: &mut Vec<(Vec<u8>, f64, Vec<FieldSpans>)>,
    buckets: &mut [Vec<RawBucket>],
) {
    for m in r.hits {
        let hl = highlight.map_or_else(Vec::new, |w| hit_highlight(ts, spec, &m.key, query, w));
        all.push((m.key, m.score, hl));
    }
    for (into, from) in buckets.iter_mut().zip(r.facets) {
        fold_raw(into, from);
    }
}

/// One hit's highlight spans as `(field name, [(start, end)])`, filtered
/// to the requested fields (`want` empty = every field with a match).
fn hit_highlight(
    ts: &kevy_text::TextSegment,
    spec: &IndexSpec,
    key: &[u8],
    query: &[u8],
    want: &[Vec<u8>],
) -> Vec<FieldSpans> {
    ts.highlight_spans(key, query)
        .into_iter()
        .filter_map(|(fi, spans)| {
            let name = spec.fields.get(fi)?.name.clone();
            if !want.is_empty() && !want.contains(&name) {
                return None;
            }
            let ranges = spans.into_iter().map(|(s, e)| (s as u32, e as u32)).collect();
            Some((name, ranges))
        })
        .collect()
}
