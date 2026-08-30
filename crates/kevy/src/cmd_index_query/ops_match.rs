//! The pass-2 MATCH page assembly: the clause construction, the hot
//! query, the cold merge, and the per-hit outputs (order keys,
//! highlight spans) the origin's reduce reads back. A child module of
//! `ops.rs` (`#[path]`), split by responsibility — `ops.rs` keeps the
//! wire faces, this file keeps the page.

use kevy_store::Store;

use super::super::args::MatchArgs;
use super::super::ops_clauses::{
    boxed_preds, distinct_field, facet_fields, scope_positions, sort_field,
};
use super::cold_seam;
use crate::index_runtime::TextColdDir;

/// One shard's pass-2 answer: its hits, and which stored-value fields
/// the origin will need each hit's key for (the sort field, the
/// distinct field).
pub(super) type ShardPage =
    (Vec<kevy_text::TextMatch>, Option<usize>, Option<usize>, Vec<Vec<kevy_text::Bucket>>);

/// A cold hit's frozen stored values, keyed by row key — what the
/// okeys/dkeys lookups read for hits the hot segment no longer holds.
pub(super) type ColdVals = std::collections::HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>;

/// Build every clause the query carries and hand them to `f`: the
/// sort/distinct/facet key closures borrow locals, so the assembly
/// and its consumers must share one scope — this continuation IS that
/// scope.
fn with_clauses<R>(
    spec: &kevy_index::IndexSpec,
    q: &MatchArgs,
    now: i64,
    f: impl FnOnce(
        &[kevy_text::Filter],
        Option<kevy_text::Sort>,
        Option<kevy_text::Distinct>,
        &[kevy_text::Facet],
    ) -> R,
) -> Result<R, Vec<u8>> {
    let tests = boxed_preds(spec, &q.filters, now)?;
    let filter: Vec<kevy_text::Filter> = tests
        .iter()
        .map(|(field, test)| kevy_text::Filter { field: *field, test: test.as_ref() })
        .collect();
    let grouped = distinct_field(spec, &q.distinct)?;
    let dkey = grouped.map(|(_, ty)| move |raw: &[u8]| kevy_index::order_key(ty, raw));
    let distinct =
        grouped.zip(dkey.as_ref()).map(|((field, _), k)| kevy_text::Distinct { field, key: k });
    let sorted = sort_field(spec, &q.sort)?;
    let key = sorted.map(|(_, _, ty)| move |raw: &[u8]| kevy_index::order_key(ty, raw));
    let sort = sorted.zip(key.as_ref()).map(|((field, desc, _), k)| kevy_text::Sort {
        field,
        desc,
        key: k,
    });
    let counted = facet_fields(spec, &q.facets)?;
    let fkeys: Vec<_> = counted
        .iter()
        .map(|(_, ty)| {
            let ty = *ty;
            move |raw: &[u8]| kevy_index::order_key(ty, raw)
        })
        .collect();
    let facets: Vec<kevy_text::Facet> = counted
        .iter()
        .zip(&fkeys)
        .map(|((field, _), k)| kevy_text::Facet { field: *field, key: k })
        .collect();
    Ok(f(&filter, sort, distinct, &facets))
}

/// This shard's hits for a pass-2 MATCH, scored against the injected
/// global statistics with every clause the query carried — the hot
/// page and the cold buckets' page, merged in the page's own order.
///
/// Fetches deep enough for the origin to skip OFFSET and still fill
/// LIMIT: a shard cannot know which of its hits survive the merge.
pub(super) fn scored_hits(
    ts: &kevy_text::TextSegment,
    spec: &kevy_index::IndexSpec,
    q: &MatchArgs,
    stats: &kevy_text::CorpusStats,
    cold: Option<&TextColdDir>,
) -> Result<(ShardPage, ColdVals), Vec<u8>> {
    let scope = scope_positions(spec, &q.scope)?;
    let sorted = sort_field(spec, &q.sort)?;
    let grouped = distinct_field(spec, &q.distinct)?;
    let now = (kevy_store::now_unix_ms() / 1000) as i64;
    with_clauses(spec, q, now, |filter, sort, distinct, facets| {
        let opts = kevy_text::QueryOpts {
            stats: Some(stats),
            typo: q.typo,
            fields: &scope,
            filter,
            sort,
            distinct,
        };
        let r = ts.matches_query_faceted(&q.text, q.limit + q.offset, opts, facets);
        let (mut hits, mut counts) = (r.hits, r.facets);
        let cold_vals = cold_seam::merge_cold_page(
            cold,
            ts,
            &q.text,
            stats,
            filter,
            sort.as_ref(),
            distinct.as_ref(),
            facets,
            q.limit + q.offset,
            &mut hits,
            &mut counts,
        );
        ((hits, sorted.map(|(f, _, _)| f), grouped.map(|(f, _)| f), counts), cold_vals)
    })
}

/// Each hit's order-preserving key for stored-value field `f` — from
/// the hot segment, or from a cold hit's frozen doc record.
pub(super) fn order_keys(
    ts: &kevy_text::TextSegment,
    spec: &kevy_index::IndexSpec,
    cold_vals: &ColdVals,
    hits: &[kevy_text::TextMatch],
    f: usize,
) -> Vec<Option<Vec<u8>>> {
    hits.iter()
        .map(|h| {
            ts.stored_value(&h.key, f)
                .map(<[u8]>::to_vec)
                .or_else(|| cold_vals.get(&h.key)?.get(f)?.clone())
                .and_then(|raw| kevy_index::order_key(spec.values[f].ty, &raw))
        })
        .collect()
}

/// One hit's highlight spans as `(field name, [(start, end)])`, filtered
/// to the requested fields (`want` empty = every field with a match).
/// Field names come from the spec, positionally aligned with the
/// segment's stored field order.
pub(super) fn hit_highlight(
    store: &mut Store,
    ts: &kevy_text::TextSegment,
    spec: &kevy_index::IndexSpec,
    key: &[u8],
    text: &[u8],
    want: &[Vec<u8>],
    chilled: bool,
) -> super::super::HitSpans {
    // A cold hit's source text lives in the ROW (the freeze consumed
    // the stored copy): with cold buckets present, empty hot spans
    // fall back to a row read-back and the same re-analysis. A hot
    // document recomputes to the same emptiness — the row IS what the
    // hot segment stored — so the fallback cannot change a hot answer.
    let mut spans = ts.highlight_spans(key, text);
    if spans.is_empty() && chilled {
        spans = cold_seam::cold_highlight(store, spec, key, text);
    }
    spans
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
