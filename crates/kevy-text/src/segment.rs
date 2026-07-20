//! [`TextSegment`] — one shard's inverted slice of one text index
//! (index-follows-key, same discipline as kevy-index's `Segment`).
//! Maintained synchronously with writes; queried with BM25 ranking
//! over shard-local statistics (per-shard df/avgdl — global
//! statistics would need cross-shard write coordination).
//!
//! The impact-bucketed posting-list structure lives in
//! [`crate::buckets`].

use std::collections::HashMap;

use crate::bm25::bm25_score;
use crate::buckets::{BAND_MIN_DL, Buckets};
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

/// One scoring candidate list: (postings, df, MaxScore upper bound).
type ScoredList<'s> = (&'s Buckets, f64, f64);

/// Per-query BM25 constants threaded through the walk helpers.
struct QueryCtx {
    n_docs: f64,
    avgdl: f64,
    limit: usize,
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
}

impl TextSegment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
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
        if let Some((old_id, old_len, old_fields)) = self.docs.remove(key) {
            self.total_len -= u64::from(old_len);
            // Remove exactly this doc's tokens, re-derived from what it
            // was indexed with — O(doc), not O(index).
            for (t, tf) in weighted_tf(&old_fields).0 {
                if let Some(list) = self.postings.get_mut(&t) {
                    list.remove(tf, old_len, old_id);
                    if list.is_empty() {
                        self.postings.remove(&t);
                    }
                }
            }
            self.id_key[old_id as usize] = None;
            self.free_ids.push(old_id);
        }
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
    }

    /// BM25-ranked matches for `query` (tokenized with the same rules;
    /// OR semantics), best `limit` hits, score-descending.
    ///
    /// MaxScore pruning: query tokens process rarest-first; once the
    /// running top-`limit` threshold exceeds the summed upper bounds
    /// of the remaining (commoner) tokens, documents seen ONLY in
    /// those lists can no longer enter — their lists are then probed
    /// per accumulated doc instead of walked. Selection is a bounded
    /// heap over borrowed keys (no per-candidate allocation).
    pub fn matches(&self, query: &[u8], limit: usize) -> Vec<TextMatch> {
        self.matches_scored(query, limit, None)
    }

    /// [`TextSegment::matches`], scored against externally-supplied
    /// corpus statistics instead of this shard's local ones.
    ///
    /// `None` uses the local stats — the shard-local BM25 that `matches`
    /// has always used, byte-identical. `Some` is the global-BM25 path:
    /// a cross-shard query aggregates each shard's `n_docs`, `avgdl` and
    /// per-query-token `df` into one [`CorpusStats`] and scores every
    /// shard against it, so hits from different shards are comparable.
    /// The MaxScore upper bound uses the same injected numbers, so
    /// pruning stays a valid bound.
    ///
    /// A query token absent from THIS shard's postings contributes no
    /// score here regardless — its documents live on other shards — so
    /// only the idf (via global df) crosses shard boundaries, never a
    /// posting.
    pub fn matches_scored(
        &self,
        query: &[u8],
        limit: usize,
        stats: Option<&CorpusStats>,
    ) -> Vec<TextMatch> {
        // Top-0 of anything is empty (same convention as kevy-vector's
        // `knn` with k = 0). Also keeps the MaxScore floor well-defined:
        // `kth_of` indexes `limit - 1`.
        if limit == 0 {
            return Vec::new();
        }
        let mut q_tokens = tokenize(query);
        q_tokens.sort();
        q_tokens.dedup();
        if q_tokens.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let (n_docs, avgdl) = self.corpus_stats(stats);
        let lists = self.scored_lists(&q_tokens, n_docs, stats);
        if lists.is_empty() {
            return Vec::new();
        }
        let ctx = QueryCtx { n_docs, avgdl, limit };
        let scores = self.accumulate(&lists, &ctx);
        self.select_top(&scores, limit)
    }

    /// Corpus `(n_docs, avgdl)`: injected global stats when supplied,
    /// this shard's local totals otherwise.
    fn corpus_stats(&self, stats: Option<&CorpusStats>) -> (f64, f64) {
        match stats {
            Some(s) => (s.n_docs, s.avgdl),
            None => {
                let n = self.docs.len() as f64;
                (n, self.total_len as f64 / n)
            }
        }
    }

    /// MaxScore accumulation: walk lists rarest-first with the tail-bound
    /// early stop, then probe the un-walked lists per accumulated doc
    /// (O(candidates) gets, never a walk of the common list — that walk
    /// was the measured 30ms p95). Returns id → score.
    fn accumulate(&self, lists: &[ScoredList<'_>], ctx: &QueryCtx) -> HashMap<u32, f64> {
        let tail_ub = tail_bounds(lists);
        let mut scores: HashMap<u32, f64> = HashMap::new();
        let mut kth_threshold = 0.0_f64;
        let mut walked = 0usize;
        for (i, (list, df, _ub)) in lists.iter().enumerate() {
            // A doc seen only in the remaining lists can't reach the
            // top-limit floor → stop WALKING; the probe loop below still
            // credits these lists to already-seen docs.
            if i > 0 && scores.len() >= ctx.limit && tail_ub[i] < kth_threshold {
                break;
            }
            walked = i + 1;
            let tail_next = tail_ub.get(i + 1).copied().unwrap_or(0.0);
            self.walk_list(list, *df, tail_next, lists.len() == 1, ctx, &mut scores);
            if scores.len() >= ctx.limit && i + 1 < lists.len() {
                kth_threshold = kth_of(&scores, ctx.limit);
            }
        }
        for (list, df, _) in &lists[walked..] {
            self.probe_list(list, *df, &[], ctx, &mut scores);
        }
        scores
    }

    /// The candidate lists for a query, rarest (highest upper bound)
    /// first. The bound is dl-independent: denom ≥ tf + k1(1-b), so
    /// score ≤ idf·tf(k1+1)/(tf + k1(1-b)).
    fn scored_lists<'s>(
        &'s self,
        q_tokens: &[Vec<u8>],
        n_docs: f64,
        stats: Option<&CorpusStats>,
    ) -> Vec<ScoredList<'s>> {
        let mut lists: Vec<ScoredList<'s>> = Vec::new();
        for t in q_tokens {
            let Some(list) = self.postings.get(t) else { continue };
            // Global df when supplied — the whole point of the injected
            // stats. Falls back to the local list length, which is what
            // the shard-local path always used.
            let df = stats
                .and_then(|s| s.df.get(t))
                .map(|&d| f64::from(d))
                .unwrap_or(list.len() as f64);
            let max_tf = f64::from(list.max_tf());
            lists.push((list, df, crate::bm25::bm25_upper(max_tf, df, n_docs)));
        }
        lists.sort_by(|a, b| b.2.total_cmp(&a.2));
        lists
    }

    /// Walk one list bucket-by-bucket (tf descending), with the
    /// bucket-level and within-bucket (single-list) early stops.
    fn walk_list(
        &self,
        list: &Buckets,
        df: f64,
        tail_next: f64,
        single: bool,
        ctx: &QueryCtx,
        scores: &mut HashMap<u32, f64>,
    ) {
        let QueryCtx { n_docs, limit, .. } = *ctx;
        let groups = list.tf_groups();
        for (bi, (tf, bands)) in groups.iter().enumerate() {
            // Bucket-level early stop: buckets are tf-descending,
            // so once even the dl-free bound of THIS tf (plus
            // everything later lists could add) can't reach the
            // kth floor, no NEW doc from here on can enter. Docs
            // already accumulated still need this list's
            // contribution — the remaining buckets are PROBED for
            // them (a key has exactly one tf per token, so no
            // double count with earlier buckets).
            if scores.len() >= limit {
                let bound = crate::bm25::bm25_upper(f64::from(*tf), df, n_docs);
                if bound + tail_next < kth_of(scores, limit) {
                    let walked_tfs: Vec<u32> =
                        groups[..bi].iter().map(|(t, _)| *t).collect();
                    self.probe_list(list, df, &walked_tfs, ctx, scores);
                    break;
                }
            }
            self.walk_bucket(*tf, bands, df, single, ctx, scores);
        }
    }

    /// Walk one tf bucket's bands (dl ascending), scoring every id.
    ///
    /// Single-list within-bucket cut: bands are dl-ASCENDING and
    /// BM25 falls as dl rises, so the band's LOWER dl edge bounds
    /// every score inside it from above. On a one-list query — each
    /// doc appears exactly ONCE in the whole list, no later
    /// contribution to lose — the first band whose bound can't beat
    /// the kth floor ends the bucket exactly. Scoring stays per-id
    /// exact via the id_dl table; bands only gate the cut.
    fn walk_bucket(
        &self,
        tf: u32,
        bands: &crate::buckets::BandsView<'_>,
        df: f64,
        single: bool,
        ctx: &QueryCtx,
        scores: &mut HashMap<u32, f64>,
    ) {
        let QueryCtx { n_docs, avgdl, limit } = *ctx;
        for (b, band) in bands.iter() {
            if band.is_empty() {
                continue;
            }
            let bound = bm25_score(
                f64::from(tf),
                df,
                n_docs,
                f64::from(BAND_MIN_DL[b as usize]),
                avgdl,
            );
            if single && scores.len() >= limit && bound < kth_of(scores, limit) {
                break;
            }
            for &id in band {
                let dl = f64::from(self.id_dl[id as usize]);
                *scores.entry(id).or_insert(0.0) +=
                    bm25_score(f64::from(tf), df, n_docs, dl, avgdl);
            }
        }
    }

    /// Contribute `list` to every ALREADY-ACCUMULATED doc via O(1)
    /// list-level probes (never a walk). A doc whose tf sits in
    /// `skip_tfs` already got this list's contribution from a WALKED
    /// bucket (tf is unique per (token, doc)) and is skipped.
    fn probe_list(
        &self,
        list: &Buckets,
        df: f64,
        skip_tfs: &[u32],
        ctx: &QueryCtx,
        scores: &mut HashMap<u32, f64>,
    ) {
        let ids: Vec<u32> = scores.keys().copied().collect();
        for &id in &ids {
            if let Some(tf) = list.get(id)
                && !skip_tfs.contains(&tf)
            {
                let dl = f64::from(self.id_dl[id as usize]);
                *scores.get_mut(&id).expect("accumulated") +=
                    bm25_score(f64::from(tf), df, ctx.n_docs, dl, ctx.avgdl);
            }
        }
    }

    /// Bounded selection: only the winners get cloned. Ids resolve
    /// to keys here — the tiebreak (key ascending) is unchanged.
    fn select_top(&self, scores: &HashMap<u32, f64>, limit: usize) -> Vec<TextMatch> {
        let key_of = |id: u32| -> &[u8] {
            self.id_key[id as usize].as_deref().expect("live posting id")
        };
        let mut top: Vec<(f64, &[u8])> = Vec::with_capacity(limit + 1);
        for (id, score) in scores {
            let cand = (*score, key_of(*id));
            if top.len() < limit {
                top.push(cand);
                if top.len() == limit {
                    top.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                }
            } else if better(cand, top[limit - 1]) {
                let pos = top.partition_point(|e| better(*e, cand));
                top.insert(pos, cand);
                top.pop();
            }
        }
        if top.len() < limit {
            top.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        }
        top.into_iter()
            .map(|(score, k)| TextMatch { key: k.to_vec(), score })
            .collect()
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

/// Strict "ranks ahead of" for (score, key) — higher score first,
/// key ascending as the tiebreak.
// float_cmp: exact equality is the tiebreak trigger — an epsilon here would
// make ranking non-deterministic for genuinely equal BM25 scores.
#[allow(clippy::float_cmp)]
fn better(a: (f64, &[u8]), b: (f64, &[u8])) -> bool {
    a.0 > b.0 || (a.0 == b.0 && a.1 < b.1)
}

/// The `limit`-th best score currently accumulated (the MaxScore
/// entry floor). O(n) selection, called only between list walks.
fn kth_of(scores: &HashMap<u32, f64>, limit: usize) -> f64 {
    let mut v: Vec<f64> = scores.values().copied().collect();
    let idx = limit - 1;
    v.select_nth_unstable_by(idx, |a, b| b.total_cmp(a));
    v[idx]
}

/// `tail_ub[i]` = Σ upper bounds of `lists[i..]`.
fn tail_bounds(lists: &[ScoredList<'_>]) -> Vec<f64> {
    let mut acc = 0.0;
    let mut v: Vec<f64> = lists
        .iter()
        .rev()
        .map(|l| {
            acc += l.2;
            acc
        })
        .collect();
    v.reverse();
    v
}

#[path = "segment_stats.rs"]
mod segment_stats;

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
