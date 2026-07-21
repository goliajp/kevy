//! Phrase / adjacency queries over the positional side-channel
//! (`crate::positions`). A child module of `segment` (declared via
//! `#[path]`), so it reaches `TextSegment`'s private fields and helpers.
//!
//! A phrase is an AND of its terms with an adjacency constraint: it
//! scores with the same BM25 sum an AND query would use, restricted to
//! documents where the tokens occur consecutively and in order. Without
//! positions (`WITH POSITIONS` was not set) nothing is verifiable, so a
//! phrase query returns empty rather than silently degrading to OR.

use std::collections::{HashMap, HashSet};

use super::{CorpusStats, TextMatch, TextSegment};
use crate::bm25::bm25_score;
use crate::positions::Positions;
use crate::token::tokenize;

impl TextSegment {
    /// BM25-ranked documents that contain `phrase`'s tokens **adjacent
    /// and in order**, best `limit` hits, score-descending.
    ///
    /// A single-token phrase is an ordinary term query (adjacency is
    /// trivial). A multi-token phrase needs the positional side-channel:
    /// on a segment created without positions it returns empty. `stats`
    /// injects global corpus statistics (the two-pass cross-shard path);
    /// `None` scores shard-local.
    pub fn phrase_matches(
        &self,
        phrase: &[u8],
        limit: usize,
        stats: Option<&CorpusStats>,
    ) -> Vec<TextMatch> {
        if limit == 0 {
            return Vec::new();
        }
        let toks = tokenize(phrase);
        match toks.len() {
            0 => return Vec::new(),
            1 => return self.matches_scored(phrase, limit, stats),
            _ => {}
        }
        let Some(pos) = self.positions.as_ref() else {
            return Vec::new();
        };
        let (n_docs, avgdl) = self.corpus_stats(stats);
        let distinct = distinct_tokens(&toks);
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for id in pos.ids(&toks[0]) {
            if doc_has_phrase(pos, &toks, id) {
                scores.insert(id, self.phrase_score(&distinct, id, stats, n_docs, avgdl));
            }
        }
        self.select_top(&scores, limit)
    }

    /// BM25 sum over the phrase's distinct tokens for one matching doc —
    /// exactly what an AND query over those terms would score it.
    fn phrase_score(
        &self,
        distinct: &[Vec<u8>],
        id: u32,
        stats: Option<&CorpusStats>,
        n_docs: f64,
        avgdl: f64,
    ) -> f64 {
        let dl = f64::from(self.id_dl[id as usize]);
        let mut score = 0.0;
        for t in distinct {
            let Some(list) = self.postings.get(t) else { continue };
            let Some(tf) = list.get(id) else { continue };
            let df = stats
                .and_then(|s| s.df.get(t))
                .map(|&d| f64::from(d))
                .unwrap_or(list.len() as f64);
            score += bm25_score(f64::from(tf), df, n_docs, dl, avgdl);
        }
        score
    }
}

/// The phrase's distinct tokens (dedup for scoring — a repeated word
/// must not be counted twice in the BM25 sum).
fn distinct_tokens(toks: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut d = toks.to_vec();
    d.sort();
    d.dedup();
    d
}

/// Whether `id`'s positions place `toks` consecutively and in order at
/// least once. Shift each token's offsets left by its phrase index and
/// intersect: a surviving offset is where one occurrence begins.
fn doc_has_phrase(pos: &Positions, toks: &[Vec<u8>], id: u32) -> bool {
    let Some(first) = pos.get(&toks[0], id) else {
        return false;
    };
    let mut starts: HashSet<u32> = first.into_iter().collect();
    for (i, t) in toks.iter().enumerate().skip(1) {
        let Some(offs) = pos.get(t, id) else {
            return false;
        };
        let shifted: HashSet<u32> =
            offs.iter().filter_map(|&p| p.checked_sub(i as u32)).collect();
        starts.retain(|s| shifted.contains(s));
        if starts.is_empty() {
            return false;
        }
    }
    true
}
