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
        let (bare, phrases, prefixes) = parse_clauses(text);
        if phrases.is_empty() && prefixes.is_empty() {
            // No phrase or prefix clause — the ordinary term query, untouched.
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
        for pfx in &prefixes {
            self.add_prefix(pfx, &mut scores, stats, n_docs, avgdl);
        }
        self.select_top(&scores, limit)
    }

    /// BM25-ranked documents holding any indexed term that begins with
    /// `prefix` — a search-as-you-type `prefix*` query, scored as the OR
    /// of its expansion terms, best `limit` hits.
    ///
    /// `prefix` is ASCII-lowercased first so it matches the stored token
    /// form (Latin tokens are lowercased on the way in). This scans the
    /// term dictionary; an ordered dictionary would binary-search to the
    /// prefix range instead — the cost it trades is one linear pass over
    /// the distinct terms, weighed against the write-path cost of keeping
    /// the dictionary ordered.
    pub fn matches_prefix(
        &self,
        prefix: &[u8],
        limit: usize,
        stats: Option<&CorpusStats>,
    ) -> Vec<TextMatch> {
        if limit == 0 || prefix.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let pfx: Vec<u8> = prefix.iter().map(u8::to_ascii_lowercase).collect();
        let (n_docs, avgdl) = self.corpus_stats(stats);
        let mut scores: HashMap<u32, f64> = HashMap::new();
        self.add_prefix(&pfx, &mut scores, stats, n_docs, avgdl);
        self.select_top(&scores, limit)
    }

    /// The terms whose document frequency a cross-shard query aggregates
    /// for global BM25: the bare tokens, every phrase's tokens, and every
    /// expansion of a `word*` prefix (expanded against THIS shard's
    /// dictionary, since which terms share the prefix is shard-local).
    /// Deduplicated. For a query with no prefix this is exactly the
    /// tokenized query, so pass 1 is unchanged.
    pub fn query_df_terms(&self, text: &[u8]) -> Vec<Vec<u8>> {
        let (bare, phrases, prefixes) = parse_clauses(text);
        let mut terms = bare;
        for phrase in &phrases {
            terms.extend(phrase.iter().cloned());
        }
        for pfx in &prefixes {
            terms.extend(self.expand_prefix(pfx).into_iter().map(<[u8]>::to_vec));
        }
        terms.sort();
        terms.dedup();
        terms
    }

    /// Add one prefix clause's contribution: the OR of every expansion
    /// term (already-lowercased `pfx` matched against the stored token
    /// form). Scanning the dictionary is the cost an ordered dictionary
    /// would replace with a binary search.
    fn add_prefix(
        &self,
        pfx: &[u8],
        scores: &mut HashMap<u32, f64>,
        stats: Option<&CorpusStats>,
        n_docs: f64,
        avgdl: f64,
    ) {
        for t in self.expand_prefix(pfx) {
            self.add_term(t, scores, stats, n_docs, avgdl);
        }
    }

    /// The dictionary terms beginning with `pfx` (already lowercased),
    /// sorted for a deterministic ranking tiebreak.
    fn expand_prefix(&self, pfx: &[u8]) -> Vec<&[u8]> {
        let mut e: Vec<&[u8]> =
            self.postings.keys().map(Vec::as_slice).filter(|t| t.starts_with(pfx)).collect();
        e.sort_unstable();
        e
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
        let (bare, phrases, prefixes) = parse_clauses(query);
        let terms: HashSet<&[u8]> = bare.iter().map(Vec::as_slice).collect();
        let mut out = Vec::new();
        for (fi, (text, _weight)) in fields.iter().enumerate() {
            let mut spans = field_spans(&tokenize_spans(text), &terms, &phrases, &prefixes);
            if !spans.is_empty() {
                spans.sort_unstable();
                spans.dedup();
                out.push((fi, spans));
            }
        }
        out
    }
}

/// Highlight spans within one field's tokens: every bare-term token, every
/// token matching a query prefix, plus the tokens of each phrase
/// occurrence (a consecutive, in-order match).
fn field_spans(
    toks: &[(Vec<u8>, usize, usize)],
    terms: &HashSet<&[u8]>,
    phrases: &[Vec<Vec<u8>>],
    prefixes: &[Vec<u8>],
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (t, s, e) in toks {
        if terms.contains(t.as_slice()) || prefixes.iter().any(|p| t.starts_with(p.as_slice())) {
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

/// Parsed query clauses: bare terms, phrases (each a token sequence) and
/// prefix stems.
type Clauses = (Vec<Vec<u8>>, Vec<Vec<Vec<u8>>>, Vec<Vec<u8>>);

/// Split a query into bare terms, quoted phrases, and `word*` prefixes.
/// A `"…"` group of two or more tokens is a phrase (a shorter group joins
/// the bare terms — a one-word "phrase" is just that word); an unquoted
/// word ending in `*` is a prefix. An unterminated quote is lenient: the
/// remainder is read as plain text rather than rejected.
fn parse_clauses(text: &[u8]) -> Clauses {
    let mut bare: Vec<Vec<u8>> = Vec::new();
    let mut phrases: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut prefixes: Vec<Vec<u8>> = Vec::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i] != b'"' {
            plain.push(text[i]);
            i += 1;
            continue;
        }
        extend_plain(&plain, &mut bare, &mut prefixes);
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
                extend_plain(&text[start..], &mut bare, &mut prefixes);
                i = text.len();
            }
        }
    }
    extend_plain(&plain, &mut bare, &mut prefixes);
    (bare, phrases, prefixes)
}

/// Split plain (unquoted) query text: a whitespace word ending in `*`
/// becomes a prefix clause (its stem, ASCII-lowercased to match the
/// stored token form), every other word tokenizes into bare terms.
fn extend_plain(plain: &[u8], bare: &mut Vec<Vec<u8>>, prefixes: &mut Vec<Vec<u8>>) {
    for word in plain.split(u8::is_ascii_whitespace) {
        match word.strip_suffix(b"*") {
            Some(stem) if !stem.is_empty() => {
                prefixes.push(stem.iter().map(u8::to_ascii_lowercase).collect());
            }
            _ => bare.extend(tokenize(word)),
        }
    }
}
