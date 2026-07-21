//! [`TextSegment`] — one shard's inverted slice of one text index
//! (index-follows-key, same discipline as kevy-index's `Segment`).
//! Maintained synchronously with writes; queried with BM25 ranking
//! over shard-local statistics (per-shard df/avgdl — global
//! statistics would need cross-shard write coordination).
//!
//! The impact-bucketed posting-list structure lives in
//! [`crate::buckets`].

use std::collections::HashMap;

use crate::buckets::Buckets;
use crate::positions::Positions;
use crate::token::tokenize;

/// One ranked hit.
#[derive(Debug, Clone, PartialEq)]
pub struct TextMatch {
    /// Row key.
    pub key: Vec<u8>,
    /// Shard-local BM25 score.
    pub score: f64,
}

/// Sizing counters (memory formula + IDX.LIST).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStats {
    /// Indexed documents.
    pub docs: u64,
    /// Distinct tokens.
    pub tokens: u64,
    /// Total postings.
    pub postings: u64,
    /// Approximate heap bytes (the measured side of the documented
    /// memory formula).
    pub approx_bytes: u64,
}

/// Corpus statistics supplied from outside a segment, for scoring one
/// shard's documents against the whole corpus rather than its own slice.
///
/// A cross-shard text query builds this by summing each shard's local
/// `n_docs` / `total_len` and, for each query token, its `df`. `df`
/// need only carry the query's tokens — the values a query actually
/// scores with — which is why global BM25 does not need a whole-corpus
/// df table.
pub struct CorpusStats {
    /// Total documents across the corpus.
    pub n_docs: f64,
    /// Mean document length (unweighted tokens) across the corpus.
    pub avgdl: f64,
    /// Global document frequency per query token; a token missing here
    /// falls back to the segment's local list length.
    pub df: std::collections::HashMap<Vec<u8>, u32>,
}

/// A field's text and the BM25 weight it was indexed at. Stored per
/// document so a removal re-derives exactly the term frequencies the
/// insert produced.
type IndexedField = (Vec<u8>, f32);

/// One document's stored form: id, unweighted length, and the fields it
/// was indexed from.
type DocRecord = (u32, u32, Vec<IndexedField>);

/// One shard's inverted segment.
///
/// `postings` maps token → (key → tf) so a pruned list is PROBED per
/// accumulated candidate (O(candidates)) instead of walked
/// (O(postings)); `docs` keeps each row's original text so an update
/// removes exactly its own tokens (re-tokenize the old text) instead
/// of scanning every posting list.
#[derive(Debug, Default)]
pub struct TextSegment {
    postings: HashMap<Vec<u8>, Buckets>,
    /// key → (doc id, dl, the field texts and the weights they were
    /// indexed with). The weights are stored rather than re-read from
    /// the spec so a removal re-derives exactly the term frequencies
    /// the insert produced.
    docs: HashMap<Vec<u8>, DocRecord>,
    /// id → key (None = freed slot, id on the free list).
    id_key: Vec<Option<Vec<u8>>>,
    /// id → dl (valid while id_key[id].is_some()).
    id_dl: Vec<u32>,
    free_ids: Vec<u32>,
    total_len: u64,
    /// Positional side-channel, present only when the index was created
    /// `WITH POSITIONS` (phrase / proximity / highlight). `None` keeps
    /// the BM25 path byte-identical to the pre-positions structure —
    /// the ranking hot path never touches it.
    positions: Option<Positions>,
}

impl TextSegment {
    /// Empty segment (ranking only, no positional postings).
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty segment that records token positions for phrase, proximity
    /// and highlight queries — the `WITH POSITIONS` form. Every other
    /// operation behaves identically; only the positional side-channel
    /// (and its memory cost) is added.
    pub fn with_positions() -> Self {
        Self { positions: Some(Positions::default()), ..Self::default() }
    }

    /// Whether this segment records token positions.
    pub fn has_positions(&self) -> bool {
        self.positions.is_some()
    }

    /// (Re-)index one row's text (`None` = row removed / excluded).
    ///
    /// Single-field sugar over [`TextSegment::apply_fields`] at neutral
    /// weight, so the two paths cannot diverge.
    pub fn apply(&mut self, key: &[u8], text: Option<&[u8]>) {
        match text {
            Some(t) => self.apply_fields(key, Some(&[(t.to_vec(), 1.0)])),
            None => self.apply_fields(key, None),
        }
    }

    /// (Re-)index one row from its declared fields, each with its BM25
    /// weight. `None` removes the row.
    ///
    /// A weight scales that field's term frequencies, so a term in a
    /// weight-3 title counts as if seen three times. Document length is
    /// summed **unweighted**: length normalisation measures how much
    /// text there is to dilute a match, and weighting it would make a
    /// heavily-weighted field penalise itself.
    pub fn apply_fields(&mut self, key: &[u8], fields: Option<&[IndexedField]>) {
        self.withdraw(key);
        let Some(fields) = fields else { return };
        let (tf_map, dl) = weighted_tf(fields);
        if tf_map.is_empty() {
            return;
        }
        let id = if let Some(id) = self.free_ids.pop() {
            self.id_key[id as usize] = Some(key.to_vec());
            self.id_dl[id as usize] = dl;
            id
        } else {
            self.id_key.push(Some(key.to_vec()));
            self.id_dl.push(dl);
            (self.id_key.len() - 1) as u32
        };
        self.docs.insert(key.to_vec(), (id, dl, fields.to_vec()));
        self.total_len += u64::from(dl);
        for (t, tf) in tf_map {
            match self.postings.entry(t) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.get_mut().insert(tf, dl, id);
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(Buckets::new_one(tf, dl, id));
                }
            }
        }
        if let Some(pos) = self.positions.as_mut() {
            for (t, offsets) in token_offsets(fields) {
                pos.set(&t, id, &offsets);
            }
        }
    }

    /// Withdraw whatever `key` was last indexed as: strip its postings
    /// and positions (re-derived from the fields it was stored with,
    /// O(doc) not O(index)) and free its id. A no-op if `key` is not
    /// indexed, so it is safe as the first step of every (re-)index.
    fn withdraw(&mut self, key: &[u8]) {
        let Some((old_id, old_len, old_fields)) = self.docs.remove(key) else {
            return;
        };
        self.total_len -= u64::from(old_len);
        for (t, tf) in weighted_tf(&old_fields).0 {
            if let Some(list) = self.postings.get_mut(&t) {
                list.remove(tf, old_len, old_id);
                if list.is_empty() {
                    self.postings.remove(&t);
                }
            }
            if let Some(pos) = self.positions.as_mut() {
                pos.remove(&t, old_id);
            }
        }
        self.id_key[old_id as usize] = None;
        self.free_ids.push(old_id);
    }
}

/// Aggregate token counts for one document's token stream.
/// Weighted term frequencies across a document's fields, plus its
/// unweighted length in tokens.
///
/// A weight multiplies the field's raw counts and the result is rounded
/// up rather than truncated: a term that occurs once in a weight-0.5
/// field still occurred, and rounding it to zero would delete a match
/// rather than de-emphasise it.
fn weighted_tf(fields: &[IndexedField]) -> (HashMap<Vec<u8>, u32>, u32) {
    let mut out: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut dl = 0u32;
    for (text, weight) in fields {
        let toks = tokenize(text);
        dl = dl.saturating_add(toks.len() as u32);
        for (t, n) in tf_of(&toks) {
            let scaled = (f64::from(n) * f64::from(*weight)).ceil().max(1.0) as u32;
            *out.entry(t).or_insert(0) = out.get(&t).copied().unwrap_or(0).saturating_add(scaled);
        }
    }
    (out, dl)
}

fn tf_of(toks: &[Vec<u8>]) -> HashMap<Vec<u8>, u32> {
    let mut tf = HashMap::new();
    for t in toks {
        *tf.entry(t.clone()).or_insert(0) += 1;
    }
    tf
}

/// Each token's ascending offsets within the document's concatenated
/// fields (field order). Positions are **unweighted** physical ordinals
/// — like `dl`, they describe where the text is, not how it is scored —
/// so a weight-3 title still advances the offset one per token.
fn token_offsets(fields: &[IndexedField]) -> HashMap<Vec<u8>, Vec<u32>> {
    let mut out: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
    let mut pos = 0u32;
    for (text, _weight) in fields {
        for tok in tokenize(text) {
            out.entry(tok).or_default().push(pos);
            pos += 1;
        }
    }
    out
}

#[path = "segment_query.rs"]
mod segment_query;

#[path = "segment_stats.rs"]
mod segment_stats;

#[path = "segment_phrase.rs"]
mod segment_phrase;

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
