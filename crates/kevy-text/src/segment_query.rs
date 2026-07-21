//! The read/query path of [`TextSegment`] — BM25-ranked `matches` with
//! MaxScore pruning — split from `segment.rs` for the 500-LOC house
//! rule. A child module (declared via `#[path]` in `segment.rs`), so it
//! reaches the segment's private fields; `corpus_stats` and `select_top`
//! are `pub(crate)` because the sibling phrase path (`segment_phrase`)
//! reuses them.
//!
//! The MaxScore machinery is unchanged from when it lived in
//! `segment.rs` — the split moves code, it does not touch the walk.

use std::collections::HashMap;

use super::{CorpusStats, TextMatch, TextSegment};
use crate::bm25::{bm25_score, bm25_upper};
use crate::buckets::{BAND_MIN_DL, BandsView, Buckets};

/// One scoring candidate list: (postings, df, MaxScore upper bound).
type ScoredList<'s> = (&'s Buckets, f64, f64);

/// Per-query BM25 constants threaded through the walk helpers.
struct QueryCtx {
    n_docs: f64,
    avgdl: f64,
    limit: usize,
}

impl TextSegment {
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
        let mut q_tokens = crate::token::tokenize(query);
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
        self.select_top(&scores, limit, &[], None)
    }

    /// Corpus `(n_docs, avgdl)`: injected global stats when supplied,
    /// this shard's local totals otherwise.
    pub(crate) fn corpus_stats(&self, stats: Option<&CorpusStats>) -> (f64, f64) {
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
            lists.push((list, df, bm25_upper(max_tf, df, n_docs)));
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
                let bound = bm25_upper(f64::from(*tf), df, n_docs);
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
        bands: &BandsView<'_>,
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
    pub(crate) fn select_top(
        &self,
        scores: &HashMap<u32, f64>,
        limit: usize,
        filter: &[crate::Filter],
        sort: Option<crate::Sort>,
    ) -> Vec<TextMatch> {
        let order = Order { desc: sort.is_some_and(|s| s.desc), sorted: sort.is_some() };
        let mut top: Vec<Cand> = Vec::with_capacity(limit + 1);
        for (id, score) in scores {
            // The candidate set is walked exactly once here, so this is
            // the cheapest correct place to test a predicate and to build
            // a sort key: testing inside each term's accumulation would
            // retest a document once per query term.
            if !self.passes(*id, filter) {
                continue;
            }
            let cand = Cand {
                score: *score,
                key: self.id_key[*id as usize].as_deref().expect("live posting id"),
                okey: sort.and_then(|s| {
                    self.values.as_ref().and_then(|dv| dv.get(*id, s.field)).and_then(s.key)
                }),
            };
            if top.len() < limit {
                top.push(cand);
                if top.len() == limit {
                    order.sort(&mut top);
                }
            } else if order.better(&cand, &top[limit - 1]) {
                let pos = top.partition_point(|e| order.better(e, &cand));
                top.insert(pos, cand);
                top.pop();
            }
        }
        if top.len() < limit {
            order.sort(&mut top);
        }
        top.into_iter().map(|c| TextMatch { key: c.key.to_vec(), score: c.score }).collect()
    }

    /// Whether `id` satisfies every predicate (they are ANDed). A
    /// document with no value for a filtered field never passes: absent
    /// is not a value, and treating it as one would let rows that simply
    /// lack the field slip through a range test.
    fn passes(&self, id: u32, filter: &[crate::Filter]) -> bool {
        if filter.is_empty() {
            return true;
        }
        let Some(dv) = self.values.as_ref() else { return false };
        filter.iter().all(|f| dv.get(id, f.field).is_some_and(|v| (f.test)(v)))
    }
}

/// Strict "ranks ahead of" for (score, key) — higher score first,
/// key ascending as the tiebreak.
// float_cmp: exact equality is the tiebreak trigger — an epsilon here would
// make ranking non-deterministic for genuinely equal BM25 scores.
#[allow(clippy::float_cmp)]
/// One candidate in the top-K selection.
struct Cand<'a> {
    score: f64,
    key: &'a [u8],
    /// The sort field's order-preserving key, when the query sorts by a
    /// stored value; `None` when it does not, or when this document has
    /// no usable value for the field.
    okey: Option<Vec<u8>>,
}

/// What the selection ranks by.
#[derive(Clone, Copy)]
struct Order {
    sorted: bool,
    desc: bool,
}

impl Order {
    /// Whether `a` outranks `b`.
    ///
    /// The row key breaks every tie, so two shards holding equally-ranked
    /// documents agree on their order and the merged page is stable.
    /// Under a sort, a document WITH a value always outranks one without,
    /// in both directions.
    fn better(self, a: &Cand, b: &Cand) -> bool {
        if !self.sorted {
            return a.score > b.score || (a.score == b.score && a.key < b.key);
        }
        match (&a.okey, &b.okey) {
            (Some(x), Some(y)) => {
                let ord = if self.desc { y.cmp(x) } else { x.cmp(y) };
                ord == std::cmp::Ordering::Less
                    || (ord == std::cmp::Ordering::Equal && a.key < b.key)
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => a.key < b.key,
        }
    }

    fn sort(self, top: &mut [Cand]) {
        top.sort_by(|a, b| {
            if self.better(a, b) {
                std::cmp::Ordering::Less
            } else if self.better(b, a) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
    }
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
