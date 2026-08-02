//! The cold-bucket seams of the two MATCH passes: the a-train
//! refusal whitelist, the pass-1 statistics merge and the pass-2 hit
//! merge. Split from `ops.rs` so the hot query bodies stay readable —
//! all three are no-ops until a windowed text index actually froze
//! something.

use crate::index_runtime::TextColdDir;

use super::super::args::MatchArgs;

/// The a-train whitelist: with cold buckets present, only bare
/// term/OR queries (plus LIMIT/OFFSET) merge; every clause whose cold
/// path has not landed refuses by name — silence would drop cold
/// documents, and these are coming in the next train.
pub(super) fn cold_refusal(q: &MatchArgs, cold: Option<&TextColdDir>) -> Option<Vec<u8>> {
    if !cold.is_some_and(|c| c.has_cold()) {
        return None;
    }
    let clauses = q.text.contains(&b'"')
        || q.text.contains(&b'*')
        || q.typo > 0
        || !q.scope.is_empty()
        || !q.filters.is_empty()
        || q.sort.is_some()
        || q.distinct.is_some()
        || !q.facets.is_empty()
        || q.highlight.is_some();
    clauses.then(|| {
        let mut chunk = vec![crate::cmd_index_query::ST_CLAUSE];
        chunk.extend_from_slice(
            b"phrases, prefixes, TYPO, IN, FILTER, SORT, DISTINCT, FACET and HIGHLIGHT \
              on a windowed text index with cold buckets are not built yet - \
              use bare terms, or query inside the hot window",
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

/// Merge the cold buckets' hits into a pass-2 fetch window. Cold hits
/// score under the SAME injected stats — one more ranked list, then
/// the window re-sorts and re-truncates.
pub(super) fn merge_cold_hits(
    cold: Option<&TextColdDir>,
    hits: &mut Vec<kevy_text::TextMatch>,
    text: &[u8],
    stats: &kevy_text::CorpusStats,
    fetch: usize,
) {
    let Some(c) = cold.filter(|c| c.has_cold()) else { return };
    let tokens = kevy_text::tokenize(text);
    hits.extend(c.cold_hits(&tokens, stats, fetch));
    hits.sort_by(|a, b| {
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.key.cmp(&b.key))
    });
    hits.truncate(fetch);
}
