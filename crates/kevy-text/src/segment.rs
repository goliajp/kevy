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
use crate::docvalues::DocValues;
use crate::fields::FieldStats;
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

/// What an index declares, in the terms a segment is built from.
#[derive(Clone, Copy, Default)]
pub struct SegmentShape {
    /// Separately scored fields — `IN <field…>` scopes to these. 0 or 1
    /// keeps no per-field breakdown, because with one field the
    /// per-field numbers are the merged ones.
    pub fields: usize,
    /// Record token positions (`WITH POSITIONS`) for phrase, proximity
    /// and adjacency-verified highlight.
    pub positions: bool,
    /// Value fields stored per document (`VALUES`), for the clauses that
    /// read a document's own value rather than a term's postings.
    pub values: usize,
}

/// A non-scoring predicate over a document's stored values.
///
/// The test takes raw bytes because this crate does not know what a
/// number or a date is; the caller coerces. A document with no value for
/// the field never passes — absent is not a value.
#[derive(Clone, Copy)]
pub struct Filter<'a> {
    /// Which declared value field the predicate reads.
    pub field: usize,
    /// The test applied to that field's bytes.
    pub test: &'a dyn Fn(&[u8]) -> bool,
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
    /// Per-field side-channel, present only on a multi-field index — the
    /// breakdown `IN <field…>` scopes to. With one field the per-field
    /// numbers are the merged ones, so a single-field index carries
    /// nothing; an unscoped query never reads it either way.
    fields: Option<FieldStats>,
    /// Stored-value side-channel, present only when the index declared
    /// value fields (`VALUES`). Answers "what is THIS document's price",
    /// which the postings cannot — see [`crate::docvalues`].
    values: Option<DocValues>,
    /// Running counters, so `stats()` never walks the index.
    /// Each mirrors one walking term of the memory formula in
    /// `segment_stats.rs` (the walker survives there as the invariant
    /// the tests hold these to). `postings_total` is the postings
    /// gauge; the other three are the segment-owned byte terms.
    postings_total: u64,
    token_bytes: u64,
    many_slots: u64,
    doc_bytes: u64,
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
        Self::with_shape(SegmentShape { positions: true, ..SegmentShape::default() })
    }

    /// Empty segment shaped by what an index declares.
    ///
    /// Each optional channel exists only when the declaration calls for
    /// it, so an index pays for what it asked for and nothing else.
    pub fn with_shape(shape: SegmentShape) -> Self {
        Self {
            positions: shape.positions.then(Positions::default),
            fields: (shape.fields > 1).then(|| FieldStats::new(shape.fields)),
            values: (shape.values > 0).then(|| DocValues::new(shape.values)),
            ..Self::default()
        }
    }

    /// Whether this segment records token positions.
    pub fn has_positions(&self) -> bool {
        self.positions.is_some()
    }

    /// How many fields this segment scores separately; 1 when it keeps no
    /// per-field breakdown (a single-field index needs none).
    pub fn field_arity(&self) -> usize {
        self.fields.as_ref().map_or(1, FieldStats::arity)
    }

    /// How many value fields this segment stores per document; 0 when it
    /// stores none.
    pub fn value_arity(&self) -> usize {
        self.values.as_ref().map_or(0, DocValues::arity)
    }

    /// Every stored value of one document by id, aligned with the
    /// declared VALUES order — what a freeze carries into a cold doc
    /// record so the value-reading clauses can serve cold hits.
    pub(crate) fn doc_values_of(&self, id: u32) -> Vec<Option<&[u8]>> {
        match self.values.as_ref() {
            Some(dv) => (0..dv.arity()).map(|f| dv.get(id, f)).collect(),
            None => Vec::new(),
        }
    }

    /// One row's stored value for a declared value field, as raw bytes.
    /// `None` when the row is not indexed here, the field was not
    /// declared, or this document has no value for it.
    ///
    /// The cross-shard merge needs each returned hit's sort value, and it
    /// is cheaper to look it up for the handful of hits a shard returns
    /// than to carry it through the ranking.
    pub fn stored_value(&self, key: &[u8], field: usize) -> Option<&[u8]> {
        let (id, _, _) = self.docs.get(key)?;
        self.values.as_ref()?.get(*id, field)
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
        self.apply_doc(key, fields, &[]);
    }

    /// [`TextSegment::apply_fields`], also storing the row's declared
    /// value fields (`VALUES`) so `FILTER` and friends can read them back
    /// per document. `values` is positional against the declaration; a
    /// short slice leaves the rest absent.
    pub fn apply_doc(
        &mut self,
        key: &[u8],
        fields: Option<&[IndexedField]>,
        values: &[Option<&[u8]>],
    ) {
        self.withdraw(key);
        let Some(fields) = fields else { return };
        let (per_field, lens) = field_tf(fields);
        let (tf_map, dl) = merge_field_tf(&per_field, &lens);
        if tf_map.is_empty() {
            return;
        }
        let id = self.take_id(key, dl);
        self.doc_bytes += doc_record_bytes(key, fields);
        self.docs.insert(key.to_vec(), (id, dl, fields.to_vec()));
        self.total_len += u64::from(dl);
        for (t, tf) in tf_map {
            self.postings_total += 1;
            match self.postings.entry(t) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let before = e.get().index_len();
                    e.get_mut().insert(tf, dl, id);
                    self.many_slots += e.get().index_len() - before;
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    self.token_bytes += v.key().len() as u64 + 48;
                    v.insert(Buckets::new_one(tf, dl, id));
                }
            }
        }
        self.index_side_channels(id, fields, &per_field, &lens);
        if let Some(dv) = self.values.as_mut() {
            dv.set(id, values);
        }
    }

    /// One indexed document's `(id, dl, weighted term→tf)` — the
    /// freeze's read half, taken BEFORE withdraw consumes the stored
    /// fields it is derived from.
    pub(crate) fn doc_terms(&self, key: &[u8]) -> Option<(u32, u32, HashMap<Vec<u8>, u32>)> {
        let (id, dl, fields) = self.docs.get(key)?;
        Some((*id, *dl, weighted_tf(fields).0))
    }

    /// The undecoded positions blob for `(term, id)`, if positions are
    /// declared and present.
    pub(crate) fn positions_blob(&self, term: &[u8], id: u32) -> Option<&[u8]> {
        self.positions.as_ref()?.blob(term, id)
    }

    /// Claim a document id for `key`, reusing a freed slot when there is
    /// one.
    fn take_id(&mut self, key: &[u8], dl: u32) -> u32 {
        if let Some(id) = self.free_ids.pop() {
            self.id_key[id as usize] = Some(key.to_vec());
            self.id_dl[id as usize] = dl;
            id
        } else {
            self.id_key.push(Some(key.to_vec()));
            self.id_dl.push(dl);
            (self.id_key.len() - 1) as u32
        }
    }

    /// Fill the physical side-channels for a freshly indexed document:
    /// token offsets for phrase / highlight, and the per-field breakdown
    /// for field-scoped scoring. Both are derived from the same single
    /// tokenisation the merged postings came from.
    fn index_side_channels(
        &mut self,
        id: u32,
        fields: &[IndexedField],
        per_field: &[HashMap<Vec<u8>, u32>],
        lens: &[u32],
    ) {
        if let Some(pos) = self.positions.as_mut() {
            for (t, offsets) in token_offsets(fields) {
                pos.set(&t, id, &offsets);
            }
        }
        let Some(fs) = self.fields.as_mut() else { return };
        fs.set_doc_len(id, lens);
        let arity = fs.arity();
        let mut by_token: HashMap<&[u8], Vec<u32>> = HashMap::new();
        for (f, m) in per_field.iter().enumerate() {
            for (t, v) in m {
                let row = by_token.entry(t).or_insert_with(|| vec![0; arity]);
                if let Some(slot) = row.get_mut(f) {
                    *slot = *v;
                }
            }
        }
        for (t, row) in by_token {
            fs.set(t, id, &row);
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
        self.doc_bytes -= doc_record_bytes(key, &old_fields);
        self.total_len -= u64::from(old_len);
        for (t, tf) in weighted_tf(&old_fields).0 {
            if let Some(list) = self.postings.get_mut(&t) {
                let before = list.index_len();
                list.remove(tf, old_len, old_id);
                self.many_slots -= before - list.index_len();
                self.postings_total -= 1;
                if list.is_empty() {
                    self.postings.remove(&t);
                    self.token_bytes -= t.len() as u64 + 48;
                }
            }
            if let Some(pos) = self.positions.as_mut() {
                pos.remove(&t, old_id);
            }
            if let Some(fs) = self.fields.as_mut() {
                fs.remove(&t, old_id);
            }
        }
        if let Some(fs) = self.fields.as_mut() {
            fs.clear_doc_len(old_id);
        }
        if let Some(dv) = self.values.as_mut() {
            dv.clear(old_id);
        }
        self.id_key[old_id as usize] = None;
        self.free_ids.push(old_id);
    }
}

/// One document's term of the docs-table byte formula: key stored
/// twice, each field's text + a small header, and the record fixed
/// cost — the exact per-entry term `recompute_stats` walks.
fn doc_record_bytes(key: &[u8], fields: &[IndexedField]) -> u64 {
    let text: usize = fields.iter().map(|(t, _)| t.len() + 4).sum();
    (2 * key.len() + text + 110) as u64
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
    let (per_field, lens) = field_tf(fields);
    merge_field_tf(&per_field, &lens)
}

/// One document's weighted term frequencies **kept per field**, plus each
/// field's unweighted length in tokens.
///
/// This is the shape the per-field channel stores and the merged postings
/// are the sum of; deriving both from one call is what guarantees the
/// scoped and unscoped paths agree on what a field contributed.
fn field_tf(fields: &[IndexedField]) -> (Vec<HashMap<Vec<u8>, u32>>, Vec<u32>) {
    let mut per_field = Vec::with_capacity(fields.len());
    let mut lens = Vec::with_capacity(fields.len());
    for (text, weight) in fields {
        let toks = tokenize(text);
        lens.push(toks.len() as u32);
        let scaled = tf_of(&toks)
            .into_iter()
            .map(|(t, n)| (t, (f64::from(n) * f64::from(*weight)).ceil().max(1.0) as u32))
            .collect();
        per_field.push(scaled);
    }
    (per_field, lens)
}

/// Fold a per-field breakdown back into the merged frequencies and length
/// the ranking postings store.
fn merge_field_tf(
    per_field: &[HashMap<Vec<u8>, u32>],
    lens: &[u32],
) -> (HashMap<Vec<u8>, u32>, u32) {
    let mut out: HashMap<Vec<u8>, u32> = HashMap::new();
    for m in per_field {
        for (t, v) in m {
            let slot = out.entry(t.clone()).or_insert(0);
            *slot = slot.saturating_add(*v);
        }
    }
    let dl = lens.iter().fold(0u32, |a, &b| a.saturating_add(b));
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

#[path = "segment_opts.rs"]
mod segment_opts;
pub use segment_opts::{Bucket, Distinct, Facet, FacetedMatches, QueryOpts, Sort};

#[path = "segment_query.rs"]
mod segment_query;
pub use segment_query::sorted_order;

#[path = "segment_stats.rs"]
mod segment_stats;

#[path = "segment_phrase.rs"]
mod segment_phrase;
pub use crate::clauses::{Clauses, parse_clauses};
pub(crate) use segment_phrase::{distinct_tokens, field_spans};

#[path = "segment_scope.rs"]
mod segment_scope;

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
