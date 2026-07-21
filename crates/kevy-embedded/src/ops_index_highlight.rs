//! Highlight-returning MATCH for the embedded API, split from
//! `ops_index.rs` for the 500-LOC house rule. A `#[path]` child module,
//! so it reaches `Store`'s crate-private index methods and fields.

use kevy_index::IndexSpec;

use super::{FieldSpans, HighlightedHit, sync_segs};
use crate::KevyResult;
use crate::store::{Store, lock_write};

impl Store {
    /// [`Self::idx_match`], additionally returning each hit's highlight
    /// spans when `highlight` is `Some` (an empty slice = every field).
    /// Highlighting re-analyses the winning document's own text, so it
    /// happens per shard alongside the score and needs no positions.
    pub fn idx_match_highlighted(
        &self,
        name: &[u8],
        query: &[u8],
        limit: usize,
        highlight: Option<&[Vec<u8>]>,
    ) -> KevyResult<Vec<HighlightedHit>> {
        let limit = limit.clamp(1, 1000);
        let stats = self.text_corpus_stats(name, query)?;
        let mut all: Vec<HighlightedHit> = Vec::new();
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            if let Some((spec, ts)) = inner.idx_segs.text.iter().find(|(s, _)| s.name == name) {
                // `matches_query` parses quoted phrases out of the raw
                // query text; with none it is the ordinary term query.
                for m in ts.matches_query(query, limit, Some(&stats)) {
                    let hl =
                        highlight.map_or_else(Vec::new, |w| hit_highlight(ts, spec, &m.key, query, w));
                    all.push((m.key, m.score, hl));
                }
            }
        }
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(limit);
        Ok(all)
    }
}

/// One hit's highlight spans filtered to the wanted fields (empty =
/// every field), field names taken from the spec's declaration order
/// (positionally aligned with the segment's stored fields).
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
