//! The cold-bucket seams of the two MATCH passes: the (narrowing)
//! refusal whitelist, the pass-1 statistics merge, and the pass-2
//! page/facet merges plus the cold highlight's row read-back. Split
//! from `ops.rs` so the hot query bodies stay readable — all of it is
//! a no-op until a windowed text index actually froze something.

use std::collections::{HashMap, HashSet};

use kevy_store::Store;

use crate::index_runtime::{ColdHit, ColdPageQuery, TextColdDir};

use super::super::args::MatchArgs;
use super::match_page::ColdVals;

/// The clauses whose cold path has not landed refuse by name —
/// silence would drop cold documents. After the b-train the refusals
/// are down to the dictionary-shaped clauses: a `word*` prefix and
/// TYPO expand against the hot term dictionary, and `IN` scopes by
/// the per-field channel — all three collapse at freeze time.
pub(super) fn cold_refusal(q: &MatchArgs, cold: Option<&TextColdDir>) -> Option<Vec<u8>> {
    if !cold.is_some_and(TextColdDir::has_cold) {
        return None;
    }
    let clauses = q.text.contains(&b'*') || q.typo > 0 || !q.scope.is_empty();
    clauses.then(|| {
        let mut chunk = vec![crate::cmd_index_query::ST_CLAUSE];
        chunk.extend_from_slice(
            b"prefixes, TYPO and IN on a windowed text index with cold buckets \
              are not built yet - drop the clause, or query inside the hot window",
        );
        chunk
    })
}

/// Merge the cold buckets into a pass-1 answer. They are pass-1
/// contributors like any other shard: docs/length come from the live
/// in-memory numbers, df from one fence descent per token (tombstoned
/// documents already withdrawn from all three).
pub(super) fn merge_cold_stats(
    cold: Option<&TextColdDir>,
    n_docs: &mut u64,
    total_len: &mut u64,
    tokdf: &mut [(Vec<u8>, u32)],
) {
    let Some(c) = cold.filter(|c| c.has_cold()) else { return };
    let tokens: Vec<Vec<u8>> = tokdf.iter().map(|(t, _)| t.clone()).collect();
    let (cn, cl, cdf) = c.cold_stats(&tokens);
    *n_docs += cn;
    *total_len += cl;
    for (t, d) in cdf {
        if let Some(e) = tokdf.iter_mut().find(|(tt, _)| *tt == t) {
            e.1 += d;
        }
    }
}

/// The whole cold seam of one pass-2 answer: build the cold page,
/// sum its facets into the hot counts, merge its hits into the hot
/// page, and hand back the frozen values the okeys/dkeys lookups
/// will need. A no-op returning empty values when nothing is frozen.
#[allow(clippy::too_many_arguments)]
pub(super) fn merge_cold_page(
    cold: Option<&TextColdDir>,
    ts: &kevy_text::TextSegment,
    text: &[u8],
    stats: &kevy_text::CorpusStats,
    filter: &[kevy_text::Filter],
    sort: Option<&kevy_text::Sort>,
    distinct: Option<&kevy_text::Distinct>,
    facets: &[kevy_text::Facet],
    fetch: usize,
    hits: &mut Vec<kevy_text::TextMatch>,
    counts: &mut [Vec<kevy_text::Bucket>],
) -> ColdVals {
    let Some(c) = cold.filter(|c| c.has_cold()) else {
        return ColdVals::new();
    };
    let page = c.cold_page(&cold_page_query(text, stats, filter, sort, distinct, facets, fetch));
    merge_facets(counts, page.facets);
    merge_hits(hits, page.hits, ts, sort, distinct, &page.values, fetch);
    page.values
}

/// What pass 2 asks of the cold directory, parsed the way the hot
/// engine parses it: bare terms sorted and deduplicated, phrases as
/// token sequences. Prefixes cannot occur — the refusal turned them
/// away before any scoring round.
#[allow(clippy::too_many_arguments)]
fn cold_page_query<'a>(
    text: &[u8],
    stats: &'a kevy_text::CorpusStats,
    filter: &'a [kevy_text::Filter<'a>],
    sort: Option<&'a kevy_text::Sort<'a>>,
    distinct: Option<&'a kevy_text::Distinct<'a>>,
    facets: &'a [kevy_text::Facet<'a>],
    fetch: usize,
) -> ColdPageQuery<'a> {
    let (mut bare, phrases, _prefixes) = kevy_text::parse_clauses(text);
    bare.sort();
    bare.dedup();
    ColdPageQuery { bare, phrases, stats, filter, sort, distinct, facets, fetch }
}

/// One merged candidate: the page-order ingredients of a hot or cold
/// hit, side by side.
struct Merged {
    key: Vec<u8>,
    score: f64,
    okey: Option<Vec<u8>>,
    dkey: Option<Vec<u8>>,
}

/// Merge the cold page into the hot one, in the page's own order —
/// `sorted_order` under SORT, score order otherwise — re-collapsing
/// under DISTINCT (the merged list is sorted best-first, so keeping
/// each identity's first occurrence keeps its best) and re-truncating
/// to the fetch window.
#[allow(clippy::too_many_arguments)]
fn merge_hits(
    hits: &mut Vec<kevy_text::TextMatch>,
    cold: Vec<ColdHit>,
    ts: &kevy_text::TextSegment,
    sort: Option<&kevy_text::Sort>,
    distinct: Option<&kevy_text::Distinct>,
    cold_vals: &HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>,
    fetch: usize,
) {
    if cold.is_empty() {
        return;
    }
    let hot = hits.drain(..).map(|h| Merged {
        okey: sort.and_then(|s| ts.stored_value(&h.key, s.field).and_then(s.key)),
        dkey: distinct.and_then(|d| ts.stored_value(&h.key, d.field).and_then(d.key)),
        key: h.key,
        score: h.score,
    });
    let chilled = cold.into_iter().map(|c| Merged {
        dkey: distinct
            .and_then(|d| cold_vals.get(&c.key)?.get(d.field)?.as_deref().and_then(d.key)),
        okey: c.okey,
        key: c.key,
        score: c.score,
    });
    let mut all: Vec<Merged> = hot.chain(chilled).collect();
    all.sort_by(|a, b| match sort {
        Some(s) => kevy_text::sorted_order(
            (a.okey.as_deref(), &a.key),
            (b.okey.as_deref(), &b.key),
            s.desc,
        ),
        None => b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key)),
    });
    if distinct.is_some() {
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        all.retain(|m| match &m.dkey {
            None => true,
            Some(id) => seen.insert(id.clone()),
        });
    }
    all.truncate(fetch);
    *hits = all.into_iter().map(|m| kevy_text::TextMatch { key: m.key, score: m.score }).collect();
}

/// Sum the cold facet counts into the hot ones by value identity —
/// the hot label wins when both halves saw a value — and restore the
/// (count desc, label asc) order `count_facet` reports in.
fn merge_facets(
    hot: &mut [Vec<kevy_text::Bucket>],
    cold: Vec<Vec<kevy_text::Bucket>>,
) {
    for (h, c) in hot.iter_mut().zip(cold) {
        if c.is_empty() {
            continue;
        }
        let mut merged: HashMap<Vec<u8>, (Vec<u8>, u64)> =
            h.drain(..).map(|(k, label, n)| (k, (label, n))).collect();
        for (k, label, n) in c {
            merged.entry(k).or_insert_with(|| (label, 0)).1 += n;
        }
        *h = merged.into_iter().map(|(k, (label, n))| (k, label, n)).collect();
        h.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    }
}

/// A cold hit's highlight spans: read the row's declared FIELD texts
/// back (`spec.read_row`'s exact shape — present fields in spec
/// order, missing skipped, the same list the hot segment stored) and
/// re-analyse with the same span rules.
pub(super) fn cold_highlight(
    store: &mut Store,
    spec: &kevy_index::IndexSpec,
    key: &[u8],
    text: &[u8],
) -> Vec<(usize, Vec<(usize, usize)>)> {
    let names: Vec<&[u8]> = spec.fields.iter().map(|f| f.name.as_slice()).collect();
    let Ok(Some(vals)) = store.peek_hash_fields(key, &names) else {
        return Vec::new();
    };
    let texts: Vec<Vec<u8>> = vals.into_iter().flatten().collect();
    kevy_text::cold::highlight_fields(&texts, text)
}
