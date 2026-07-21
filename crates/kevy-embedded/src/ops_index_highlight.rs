//! Clause-carrying MATCH for the embedded API, split from
//! `ops_index.rs` for the 500-LOC house rule. A `#[path]` child module,
//! so it reaches `Store`'s crate-private index methods and fields.

use kevy_index::IndexSpec;

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
        let limit = limit.clamp(1, 1000);
        let offset = opts.offset.min(10_000);
        // Fetch deep enough to skip OFFSET and still fill LIMIT after the
        // cross-shard merge.
        let fetch = limit + offset;
        let scope = self.scope_positions(name, opts.scope)?;
        let stats = self.text_corpus_stats_in(name, query, opts.typo, &scope)?;
        let mut all: Vec<HighlightedHit> = Vec::new();
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((spec, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                // `matches_query_with` parses quoted phrases out of the
                // raw query text; with none it is the ordinary term query.
                let q = kevy_text::QueryOpts {
                    stats: Some(&stats),
                    typo: opts.typo,
                    fields: &scope,
                };
                for m in ts.matches_query_with(query, fetch, q) {
                    let hl = opts
                        .highlight
                        .map_or_else(Vec::new, |w| hit_highlight(ts, spec, &m.key, query, w));
                    all.push((m.key, m.score, hl));
                }
            }
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if offset > 0 {
            all.drain(..offset.min(all.len()));
        }
        all.truncate(limit);
        Ok(all)
    }

    /// Map an `IN <field…>` clause's names onto the index's field
    /// positions, in declaration order. Errors — naming what the index
    /// does actually index — when a name is not declared.
    fn scope_positions(&self, name: &[u8], scope: &[Vec<u8>]) -> KevyResult<Vec<usize>> {
        if scope.is_empty() {
            return Ok(Vec::new());
        }
        let guard = self.indexes.catalog.read().unwrap_or_else(|e| e.into_inner());
        let Some((spec, _)) = guard.1.get(name) else {
            return Err(KevyError::NotFound("no such text index".into()));
        };
        scope
            .iter()
            .map(|want| {
                spec.fields.iter().position(|f| f.name == *want).ok_or_else(|| {
                    let declared: Vec<String> = spec
                        .fields
                        .iter()
                        .map(|f| String::from_utf8_lossy(&f.name).into_owned())
                        .collect();
                    KevyError::InvalidInput(
                        format!(
                            "IN names field '{}', which this index does not declare — it indexes: {}",
                            String::from_utf8_lossy(want),
                            declared.join(", ")
                        ),
                    )
                })
            })
            .collect()
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
