//! [`TextSegment`] — one shard's inverted slice of one text index
//! (index-follows-key, same discipline as kevy-index's `Segment`).
//! Maintained synchronously with writes; queried with BM25 ranking
//! over shard-local statistics (RFC D2: per-shard df/avgdl — global
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
    /// Approximate heap bytes (RFC D4 formula's measured side).
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
    /// key → (doc id, dl, original text).
    docs: HashMap<Vec<u8>, (u32, u32, Vec<u8>)>,
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
    pub fn apply(&mut self, key: &[u8], text: Option<&[u8]>) {
        if let Some((old_id, old_len, old_text)) = self.docs.remove(key) {
            self.total_len -= u64::from(old_len);
            // Remove exactly this doc's tokens (re-derive tf from the
            // old text — O(doc), not O(index)).
            for (t, tf) in tf_of(&tokenize(&old_text)) {
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
        let Some(text) = text else { return };
        let toks = tokenize(text);
        if toks.is_empty() {
            return;
        }
        let dl = toks.len() as u32;
        let id = match self.free_ids.pop() {
            Some(id) => {
                self.id_key[id as usize] = Some(key.to_vec());
                self.id_dl[id as usize] = dl;
                id
            }
            None => {
                self.id_key.push(Some(key.to_vec()));
                self.id_dl.push(dl);
                (self.id_key.len() - 1) as u32
            }
        };
        self.docs.insert(key.to_vec(), (id, dl, text.to_vec()));
        self.total_len += u64::from(dl);
        for (t, tf) in tf_of(&toks) {
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
        let mut q_tokens = tokenize(query);
        q_tokens.sort();
        q_tokens.dedup();
        if q_tokens.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let n_docs = self.docs.len() as f64;
        let avgdl = self.total_len as f64 / n_docs;
        let lists = self.scored_lists(&q_tokens, n_docs);
        if lists.is_empty() {
            return Vec::new();
        }
        let tail_ub = tail_bounds(&lists);
        let ctx = QueryCtx { n_docs, avgdl, limit };
        let mut scores: HashMap<u32, f64> = HashMap::new();
        let mut kth_threshold = 0.0_f64;
        let mut walked = 0usize;
        for (i, (list, df, _ub)) in lists.iter().enumerate() {
            // Docs appearing only in the remaining lists can't reach
            // the current top-limit floor → stop WALKING; the loop
            // below PROBES these lists for already-seen docs.
            if i > 0 && scores.len() >= limit && tail_ub[i] < kth_threshold {
                break;
            }
            walked = i + 1;
            let tail_next = tail_ub.get(i + 1).copied().unwrap_or(0.0);
            self.walk_list(list, *df, tail_next, lists.len() == 1, &ctx, &mut scores);
            if scores.len() >= limit && i + 1 < lists.len() {
                kth_threshold = kth_of(&scores, limit);
            }
        }
        // Probe un-walked lists PER ACCUMULATED DOC — O(candidates)
        // hash gets, never a walk of the common list (walking here
        // was the measured 30ms p95: a pruned 500k-posting head list
        // still cost a full scan).
        for (list, df, _) in &lists[walked..] {
            self.probe_list(list, *df, &[], &ctx, &mut scores);
        }
        self.select_top(&scores, limit)
    }

    /// The candidate lists for a query, rarest (highest upper bound)
    /// first. The bound is dl-independent: denom ≥ tf + k1(1-b), so
    /// score ≤ idf·tf(k1+1)/(tf + k1(1-b)).
    fn scored_lists<'s>(&'s self, q_tokens: &[Vec<u8>], n_docs: f64) -> Vec<ScoredList<'s>> {
        let mut lists: Vec<ScoredList<'s>> = Vec::new();
        for t in q_tokens {
            let Some(list) = self.postings.get(t) else { continue };
            let df = list.len() as f64;
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
    /// v3.5 single-list within-bucket cut: bands are dl-ASCENDING and
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

    /// Live counters.
    pub fn stats(&self) -> TextStats {
        let postings: u64 = self.postings.values().map(|l| l.len() as u64).sum();
        // Hapax lists are INLINE (enum One) — no heap beyond their
        // postings-map slot; only Many lists pay the per-posting
        // band-vec + index costs.
        let many_postings: u64 = self
            .postings
            .values()
            .map(|l| match l {
                Buckets::One { .. } => 0,
                Buckets::Many(m) => m.index.len() as u64,
            })
            .sum();
        let token_bytes: u64 = self.postings.keys().map(|t| (t.len() + 48) as u64).sum();
        // docs table + the id→key/id→dl tables (key stored twice).
        let doc_bytes: u64 = self
            .docs
            .iter()
            .map(|(k, (_, _, text))| (2 * k.len() + text.len() + 110) as u64)
            .sum();
        TextStats {
            docs: self.docs.len() as u64,
            tokens: self.postings.len() as u64,
            postings,
            // per-Many-posting ≈ 4B band-vec slot + ~26B list-index
            // entry (doc-id postings + log2 dl bands, v3.5); hapax
            // lists are inline. Docs keep their original text
            // (update path re-derives tokens).
            approx_bytes: token_bytes + many_postings * 30 + doc_bytes,
        }
    }

    /// Verify hook: is `key` indexed here?
    pub fn contains(&self, key: &[u8]) -> bool {
        self.docs.contains_key(key)
    }
}

/// Aggregate token counts for one document's token stream.
fn tf_of(toks: &[Vec<u8>]) -> HashMap<Vec<u8>, u32> {
    let mut tf = HashMap::new();
    for t in toks {
        *tf.entry(t.clone()).or_insert(0) += 1;
    }
    tf
}

/// Strict "ranks ahead of" for (score, key) — higher score first,
/// key ascending as the tiebreak.
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

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
