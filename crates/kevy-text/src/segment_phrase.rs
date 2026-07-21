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
use crate::token::{tokenize, tokenize_spans};

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
        let Some(anchor) = self.rarest_anchor(&toks) else {
            return Vec::new();
        };
        let (n_docs, avgdl) = self.corpus_stats(stats);
        let distinct = distinct_tokens(&toks);
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for id in pos.ids(anchor) {
            if doc_has_phrase(pos, &toks, id) {
                scores.insert(id, self.phrase_score(&distinct, id, stats, n_docs, avgdl));
            }
        }
        self.select_top(&scores, limit)
    }

    /// BM25-ranked matches for a query `text` that may mix bare terms and
    /// double-quoted phrases (`foo "quick brown" bar`), best `limit` hits.
    ///
    /// The query is the OR of its clauses — each bare term and each
    /// phrase — scored by the summed BM25 an OR query would give, with a
    /// phrase clause contributing only to documents where its tokens are
    /// adjacent. With no quoted phrase in `text` this is byte-identical
    /// to [`TextSegment::matches_scored`] (the pruned hot path); the
    /// phrase branch trades that pruning for exactness and is what the
    /// positional side-channel exists for. `stats` injects global corpus
    /// statistics (the cross-shard path); `None` scores shard-local.
    pub fn matches_query(
        &self,
        text: &[u8],
        limit: usize,
        stats: Option<&CorpusStats>,
    ) -> Vec<TextMatch> {
        if limit == 0 {
            return Vec::new();
        }
        let (bare, phrases) = parse_clauses(text);
        if phrases.is_empty() {
            // No phrase clause — the ordinary term query, untouched.
            return self.matches_scored(text, limit, stats);
        }
        if self.docs.is_empty() {
            return Vec::new();
        }
        let (n_docs, avgdl) = self.corpus_stats(stats);
        let mut terms = bare;
        terms.sort();
        terms.dedup();
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for t in &terms {
            self.add_term(t, &mut scores, stats, n_docs, avgdl);
        }
        for phrase in &phrases {
            self.add_phrase(phrase, &mut scores, stats, n_docs, avgdl);
        }
        self.select_top(&scores, limit)
    }

    /// Add one bare term's BM25 contribution to every document that holds
    /// it — a full-list walk (no MaxScore pruning), used only on the
    /// phrase path where a phrase clause could otherwise boost a document
    /// pruning would have dropped.
    fn add_term(
        &self,
        t: &[u8],
        scores: &mut HashMap<u32, f64>,
        stats: Option<&CorpusStats>,
        n_docs: f64,
        avgdl: f64,
    ) {
        let Some(list) = self.postings.get(t) else { return };
        let df = stats
            .and_then(|s| s.df.get(t))
            .map(|&d| f64::from(d))
            .unwrap_or(list.len() as f64);
        for (tf, bands) in list.tf_groups() {
            for (_b, band) in bands.iter() {
                for &id in band {
                    let dl = f64::from(self.id_dl[id as usize]);
                    *scores.entry(id).or_insert(0.0) +=
                        bm25_score(f64::from(tf), df, n_docs, dl, avgdl);
                }
            }
        }
    }

    /// Add one phrase clause's contribution: for every document whose
    /// positions place the phrase adjacently, the BM25 sum of its tokens.
    /// A segment without positions can verify nothing, so the clause
    /// contributes to no document.
    fn add_phrase(
        &self,
        toks: &[Vec<u8>],
        scores: &mut HashMap<u32, f64>,
        stats: Option<&CorpusStats>,
        n_docs: f64,
        avgdl: f64,
    ) {
        let Some(pos) = self.positions.as_ref() else { return };
        let Some(anchor) = self.rarest_anchor(toks) else { return };
        let distinct = distinct_tokens(toks);
        for id in pos.ids(anchor) {
            if doc_has_phrase(pos, toks, id) {
                *scores.entry(id).or_insert(0.0) +=
                    self.phrase_score(&distinct, id, stats, n_docs, avgdl);
            }
        }
    }

    /// The phrase token with the fewest postings — the tightest candidate
    /// set, since every phrase token must appear in a matching document,
    /// so anchoring the scan on the rarest one avoids walking a head
    /// term's whole list. `None` if any token is absent (the phrase then
    /// matches nothing).
    fn rarest_anchor<'a>(&self, toks: &'a [Vec<u8>]) -> Option<&'a [u8]> {
        let mut best: Option<(&'a [u8], usize)> = None;
        for t in toks {
            let df = self.postings.get(t)?.len();
            if best.is_none_or(|(_, b)| df < b) {
                best = Some((t, df));
            }
        }
        best.map(|(t, _)| t)
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

impl TextSegment {
    /// Byte spans in `key`'s stored fields where `query` matched: a bare
    /// term highlights every occurrence, a phrase only its adjacent runs.
    /// Returns `(field_index, spans)` for each field with a match, each
    /// span list sorted and de-duplicated. Empty when `key` is not
    /// indexed.
    ///
    /// It re-analyses the winning document's own text — the fields are
    /// stored for re-indexing already — so it needs no positional
    /// side-channel: highlighting a handful of hits is cheap.
    pub fn highlight_spans(&self, key: &[u8], query: &[u8]) -> Vec<(usize, Vec<(usize, usize)>)> {
        let Some((_, _, fields)) = self.docs.get(key) else {
            return Vec::new();
        };
        let (bare, phrases) = parse_clauses(query);
        let terms: HashSet<&[u8]> = bare.iter().map(Vec::as_slice).collect();
        let mut out = Vec::new();
        for (fi, (text, _weight)) in fields.iter().enumerate() {
            let mut spans = field_spans(&tokenize_spans(text), &terms, &phrases);
            if !spans.is_empty() {
                spans.sort_unstable();
                spans.dedup();
                out.push((fi, spans));
            }
        }
        out
    }
}

/// Highlight spans within one field's tokens: every bare-term token, plus
/// the tokens of each phrase occurrence (a consecutive, in-order match).
fn field_spans(
    toks: &[(Vec<u8>, usize, usize)],
    terms: &HashSet<&[u8]>,
    phrases: &[Vec<Vec<u8>>],
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (t, s, e) in toks {
        if terms.contains(t.as_slice()) {
            spans.push((*s, *e));
        }
    }
    for phrase in phrases {
        let last = toks.len().saturating_sub(phrase.len() - 1);
        for start in 0..last {
            if (0..phrase.len()).all(|k| toks[start + k].0 == phrase[k]) {
                for (_, s, e) in &toks[start..start + phrase.len()] {
                    spans.push((*s, *e));
                }
            }
        }
    }
    spans
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

/// Split a query into bare terms and quoted phrases. A `"…"` group whose
/// tokens number two or more is a phrase clause; a shorter group (empty
/// or one token) is not a phrase, so its token joins the bare terms — a
/// one-word "phrase" is just that word. An unterminated quote is lenient:
/// the remainder is read as bare terms rather than rejected.
fn parse_clauses(text: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<Vec<u8>>>) {
    let mut bare: Vec<Vec<u8>> = Vec::new();
    let mut phrases: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i] != b'"' {
            plain.push(text[i]);
            i += 1;
            continue;
        }
        bare.extend(tokenize(&plain));
        plain.clear();
        let start = i + 1;
        match text[start..].iter().position(|&b| b == b'"') {
            Some(off) => {
                let toks = tokenize(&text[start..start + off]);
                if toks.len() >= 2 {
                    phrases.push(toks);
                } else {
                    bare.extend(toks);
                }
                i = start + off + 1;
            }
            None => {
                bare.extend(tokenize(&text[start..]));
                i = text.len();
            }
        }
    }
    bare.extend(tokenize(&plain));
    (bare, phrases)
}
