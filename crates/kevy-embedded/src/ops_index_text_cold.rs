//! The embedded MATCH's cold seams — the mirror of the server's
//! `ops_cold.rs` over the shared [`kevy_window::TextColdDir`]: the
//! refusal whitelist, the pass-2 cold page gathered into the shard
//! union, and the cold hit's highlight row read-back. A `#[path]`
//! child of `ops_index.rs`, compiled with the `text` feature.

#![cfg(feature = "text")]

#[cfg(not(target_arch = "wasm32"))]
use crate::store::lock_write;
use crate::store::Store;
use crate::{KevyError, KevyResult};

/// A cold hit's frozen stored values, keyed by row key — what the
/// union's sort/distinct keys read for hits no hot segment holds.
pub(crate) type ColdVals = std::collections::HashMap<Vec<u8>, Vec<Option<Vec<u8>>>>;

impl crate::ops_index::ShardSegs {
    /// The cold text directory for `name`, if that text index belongs
    /// to a windowed table AND a tick has reconciled it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn cold_text_of(&self, name: &[u8]) -> Option<&kevy_window::TextColdDir> {
        self.cold_text.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }
}

impl Store {
    /// Whether any shard's cold directory for `name` holds frozen
    /// buckets — the refusal gate's probe.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn text_has_cold(&self, name: &[u8]) -> bool {
        self.shards.iter().any(|shard| {
            lock_write(shard).idx_segs.cold_text_of(name).is_some_and(|d| d.has_cold())
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn text_has_cold(&self, _name: &[u8]) -> bool {
        false
    }
}

/// Fold one shard's frozen buckets into the pass-1 accumulators:
/// live docs/length from memory, per-token df from one fence descent
/// per segment (tombstoned documents already withdrawn from all
/// three).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fold_cold_stats(
    dir: Option<&kevy_window::TextColdDir>,
    tokdf: &[(Vec<u8>, u32)],
    n_docs: &mut f64,
    total_len: &mut u64,
    df: &mut std::collections::HashMap<Vec<u8>, u32>,
) {
    let Some(dir) = dir.filter(|d| d.has_cold()) else { return };
    let tokens: Vec<Vec<u8>> = tokdf.iter().map(|(t, _)| t.clone()).collect();
    let (cn, cl, cdf) = dir.cold_stats(&tokens);
    *n_docs += cn as f64;
    *total_len += cl;
    for (t, d) in cdf {
        *df.entry(t).or_insert(0) += d;
    }
}

/// The clauses whose cold path has not landed refuse by name — the
/// server's exact whitelist: a `word*` prefix and TYPO expand against
/// the hot term dictionary, and `IN` scopes by the per-field channel,
/// all three collapsed at freeze time.
pub(crate) fn cold_refusal(
    has_cold: bool,
    query: &[u8],
    typo: u32,
    scope: &[Vec<u8>],
) -> KevyResult<()> {
    if has_cold && (query.contains(&b'*') || typo > 0 || !scope.is_empty()) {
        return Err(KevyError::InvalidInput(
            "prefixes, TYPO and IN on a windowed text index with cold buckets \
             are not built yet — drop the clause, or query inside the hot window"
                .into(),
        ));
    }
    Ok(())
}

/// A cold hit's highlight spans: the source text lives in the ROW
/// (the freeze consumed the stored copy) — read the declared FIELD
/// texts back in `spec.read_row`'s exact shape and re-analyse with
/// the same span rules the hot path uses.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn cold_hit_highlight(
    store: &mut kevy_store::Store,
    spec: &kevy_index::IndexSpec,
    key: &[u8],
    query: &[u8],
    want: &[Vec<u8>],
) -> Vec<crate::ops_index::FieldSpans> {
    let names: Vec<&[u8]> = spec.fields.iter().map(|f| f.name.as_slice()).collect();
    let Ok(Some(vals)) = store.peek_hash_fields(key, &names) else {
        return Vec::new();
    };
    let texts: Vec<Vec<u8>> = vals.into_iter().flatten().collect();
    kevy_text::cold::highlight_fields(&texts, query)
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
