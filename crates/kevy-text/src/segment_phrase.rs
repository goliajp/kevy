//! What a query *means* — clause parsing plus the phrase / prefix /
//! typo / field-scoped entry points. A child module of `segment`
//! (declared via `#[path]`), so it reaches `TextSegment`'s private
//! fields and helpers. What each clause *contributes* to a score lives
//! next door in `segment_scope`.
//!
//! A phrase is an AND of its terms with an adjacency constraint: it
//! scores with the same BM25 sum an AND query would use, restricted to
//! documents where the tokens occur consecutively and in order. Without
//! positions (`WITH POSITIONS` was not set) nothing is verifiable, so a
//! phrase query returns empty rather than silently degrading to OR.

use std::collections::{HashMap, HashSet};

use super::segment_scope::Scope;
use super::{CorpusStats, QueryOpts, TextMatch, TextSegment};
use crate::positions::{Positions, walk};
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
        if self.positions.is_none() {
            return Vec::new();
        }
        let (n_docs, avgdl) = self.corpus_stats(stats);
        let sc = Scope { stats, n_docs, avgdl, want: &[] };
        let mut scores: HashMap<u32, f64> = HashMap::new();
        self.add_phrase(&toks, &mut scores, &sc);
        self.select_top(&scores, limit, &[], None, None)
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
        self.matches_query_typo(text, limit, stats, 0)
    }

    /// [`TextSegment::matches_query`] with a typo budget: each bare term
    /// also matches the dictionary terms within `typo` edits of it
    /// (`TYPO n`). A budget of 0 is the exact query, byte-identical.
    ///
    /// Only bare terms are fuzzed — a phrase asks for those exact tokens
    /// adjacent, and a prefix is already an inexact match, so widening
    /// either would answer a question the user did not ask.
    pub fn matches_query_typo(
        &self,
        text: &[u8],
        limit: usize,
        stats: Option<&CorpusStats>,
        typo: u32,
    ) -> Vec<TextMatch> {
        self.matches_query_with(text, limit, QueryOpts { stats, typo, ..QueryOpts::default() })
    }

    /// [`TextSegment::matches_query`] with every option a MATCH carries:
    /// injected corpus statistics, a typo budget, the field positions the
    /// query is restricted to (`IN <field…>`, empty = every field), and
    /// the non-scoring predicates it must satisfy (`FILTER`).
    ///
    /// A scoped query is a *field-scoped BM25*, not a filter over
    /// whole-document scores: frequency, length and document frequency
    /// all come from the wanted fields alone, so a match in a short title
    /// is not diluted by a long body that never mentioned the term.
    pub fn matches_query_with(
        &self,
        text: &[u8],
        limit: usize,
        opts: QueryOpts,
    ) -> Vec<TextMatch> {
        self.matches_query_faceted(text, limit, opts, &[]).hits
    }

    /// [`TextSegment::matches_query_with`], additionally counting the
    /// values of stored fields over the **whole match set**.
    ///
    /// Counted before the top-K, because a facet is about what matched
    /// and the page is only `limit` of it. `FILTER` restricts the count —
    /// a filtered-out document did not match — but `DISTINCT` does not:
    /// collapsing decides which documents are shown, not which matched.
    pub fn matches_query_faceted(
        &self,
        text: &[u8],
        limit: usize,
        opts: QueryOpts,
        facets: &[crate::Facet],
    ) -> crate::FacetedMatches {
        let empty = || crate::FacetedMatches { hits: Vec::new(), facets: vec![Vec::new(); facets.len()] };
        if limit == 0 {
            return empty();
        }
        let Some(want) = self.normalize_scope(opts.fields) else {
            return empty();
        };
        let (bare, phrases, prefixes) = parse_clauses(text);
        if phrases.is_empty()
            && prefixes.is_empty()
            && opts.typo == 0
            && want.is_empty()
            && opts.filter.is_empty()
            && opts.sort.is_none()
            && opts.distinct.is_none()
            && facets.is_empty()
        {
            // No phrase, prefix, typo, field, filter, sort or distinct
            // clause — the
            // ordinary pruned term query.
            return crate::FacetedMatches {
                hits: self.matches_scored(text, limit, opts.stats),
                facets: Vec::new(),
            };
        }
        // A filtered or sorted query takes the full walk deliberately.
        // MaxScore
        // prunes against the k-th best score SO FAR, computed over
        // unfiltered candidates: if the unfiltered leaders are the ones
        // the predicate rejects, the qualifying documents behind them may
        // never be accumulated at all. Pruning would not merely rank them
        // wrongly — it would lose them. A sort is the same hazard read
        // the other way: the buckets are ordered by score, which under
        // SORT is not what decides the page at all.
        if self.docs.is_empty() {
            return empty();
        }
        let scores = self.accumulate_clauses(bare, &phrases, &prefixes, &want, &opts);
        crate::FacetedMatches {
            facets: facets.iter().map(|f| self.count_facet(&scores, opts.filter, *f)).collect(),
            hits: self.select_top(&scores, limit, opts.filter, opts.sort, opts.distinct),
        }
    }

    /// Every clause's BM25 contribution, accumulated over the whole
    /// candidate set — the un-pruned walk the phrase, prefix, typo,
    /// field-scoped, filtered and sorted paths all share.
    fn accumulate_clauses(
        &self,
        bare: Vec<Vec<u8>>,
        phrases: &[Vec<Vec<u8>>],
        prefixes: &[Vec<u8>],
        want: &[usize],
        opts: &QueryOpts,
    ) -> HashMap<u32, f64> {
        let (n_docs, avgdl) = self.scope_stats(opts.stats, want);
        let sc = Scope { stats: opts.stats, n_docs, avgdl, want };
        let mut terms = bare;
        terms.sort();
        terms.dedup();
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for t in &terms {
            self.add_typo(t, opts.typo, &mut scores, &sc);
        }
        for phrase in phrases {
            self.add_phrase(phrase, &mut scores, &sc);
        }
        for pfx in prefixes {
            self.add_prefix(pfx, &mut scores, &sc);
        }
        scores
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
        let sc = Scope { stats, n_docs, avgdl, want: &[] };
        let mut scores: HashMap<u32, f64> = HashMap::new();
        self.add_prefix(&pfx, &mut scores, &sc);
        self.select_top(&scores, limit, &[], None, None)
    }

    /// The terms whose document frequency a cross-shard query aggregates
    /// for global BM25: the bare tokens, every phrase's tokens, and every
    /// expansion of a `word*` prefix (expanded against THIS shard's
    /// dictionary, since which terms share the prefix is shard-local).
    /// Deduplicated. For a query with no prefix this is exactly the
    /// tokenized query, so pass 1 is unchanged.
    pub fn query_df_terms(&self, text: &[u8]) -> Vec<Vec<u8>> {
        self.query_df_terms_typo(text, 0)
    }

    /// [`TextSegment::query_df_terms`] with a typo budget, so a fuzzed
    /// term's neighbours get their df aggregated globally too.
    pub fn query_df_terms_typo(&self, text: &[u8], typo: u32) -> Vec<Vec<u8>> {
        let (bare, phrases, prefixes) = parse_clauses(text);
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for t in &bare {
            if typo == 0 {
                terms.push(t.clone());
            } else {
                terms.extend(self.expand_typo(t, typo).into_iter().map(<[u8]>::to_vec));
            }
        }
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

    /// The document frequency this shard contributes for each of a
    /// query's terms, over the query's field scope.
    ///
    /// Unscoped this is the ordinary posting-list length. Scoped it is
    /// the number of documents holding the term *in the wanted fields* —
    /// counted by the same walk that would score them, because summing
    /// stored per-field counts would count a document twice when it holds
    /// the term in two of the fields.
    pub fn query_df_in(&self, text: &[u8], opts: QueryOpts) -> Vec<(Vec<u8>, u32)> {
        let want = self.normalize_scope(opts.fields).unwrap_or_default();
        self.query_df_terms_typo(text, opts.typo)
            .into_iter()
            .map(|t| {
                let df = match self.fields.as_ref() {
                    Some(fs) if !want.is_empty() => fs.docs_in(&t, &want).len(),
                    _ => self.postings.get(&t).map_or(0, super::Buckets::len),
                };
                (t, df as u32)
            })
            .collect()
    }

    /// The field positions a query is really scoped to, or `None` when
    /// the scope cannot match anything in this segment.
    ///
    /// A single-field segment keeps no per-field channel because it needs
    /// none: scoping to its only field *is* the unscoped query, and
    /// scoping to any other position matches nothing.
    fn normalize_scope(&self, want: &[usize]) -> Option<Vec<usize>> {
        let mut w = want.to_vec();
        w.sort_unstable();
        w.dedup();
        if w.is_empty() {
            return Some(Vec::new());
        }
        if self.fields.is_none() {
            return (w == [0]).then(Vec::new);
        }
        Some(w)
    }

    /// Add one phrase clause's contribution: for every document whose
    /// positions place the phrase adjacently, the BM25 sum of its tokens.
    /// A segment without positions can verify nothing, so the clause
    /// contributes to no document.
    fn add_phrase(&self, toks: &[Vec<u8>], scores: &mut HashMap<u32, f64>, sc: &Scope) {
        let Some(pos) = self.positions.as_ref() else { return };
        let Some(anchor) = self.rarest_anchor(toks) else { return };
        let distinct = distinct_tokens(toks);
        for id in pos.ids(anchor) {
            if self.phrase_hit(pos, toks, id, sc) {
                *scores.entry(id).or_insert(0.0) += self.clause_score(&distinct, id, sc);
            }
        }
    }

    /// Whether `id` contains the phrase — and, when the query is scoped,
    /// contains it *inside* one of the wanted fields rather than
    /// somewhere else in the document.
    fn phrase_hit(&self, pos: &Positions, toks: &[Vec<u8>], id: u32, sc: &Scope) -> bool {
        // Unscoped only needs "does it occur", which is answerable without
        // materialising where.
        if !sc.scoped() {
            return phrase_occurs(pos, toks, id);
        }
        let starts = phrase_starts(pos, toks, id);
        let len = toks.len() as u32;
        starts.iter().any(|&s| self.phrase_in_scope(id, s, len, sc.want))
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
/// least once — the same question [`phrase_starts`] answers, without
/// building any of the answer.
///
/// Allocation-free on purpose. A profile of a two-head-term phrase over
/// a million documents spends 87% of its self time in the allocator, and
/// this is where those allocations were: `Positions::get` decodes a blob
/// into a fresh `Vec` once per candidate document per token. Walking the
/// bytes in place removes both.
///
/// Re-walking a later token's blob per candidate start looks quadratic
/// and is not, in the shape that matters: a blob holds ONE document's
/// occurrences of ONE token, which is almost always one or two. The scan
/// short-circuits on the first occurrence found.
fn phrase_occurs(pos: &Positions, toks: &[Vec<u8>], id: u32) -> bool {
    let Some(first) = pos.blob(&toks[0], id) else {
        return false;
    };
    walk(first).any(|start| {
        toks.iter().enumerate().skip(1).all(|(i, t)| {
            pos.blob(t, id).is_some_and(|b| walk(b).any(|p| p == start + i as u32))
        })
    })
}

/// Where in `id`'s token stream `toks` occur consecutively and in order.
/// Shift each token's offsets left by its phrase index and intersect: a
/// surviving offset is where one occurrence begins. Empty = no
/// occurrence, which is also what a scoped query filters further.
fn phrase_starts(pos: &Positions, toks: &[Vec<u8>], id: u32) -> HashSet<u32> {
    let Some(first) = pos.get(&toks[0], id) else {
        return HashSet::new();
    };
    let mut starts: HashSet<u32> = first.into_iter().collect();
    for (i, t) in toks.iter().enumerate().skip(1) {
        let Some(offs) = pos.get(t, id) else {
            return HashSet::new();
        };
        let shifted: HashSet<u32> =
            offs.iter().filter_map(|&p| p.checked_sub(i as u32)).collect();
        starts.retain(|s| shifted.contains(s));
        if starts.is_empty() {
            return starts;
        }
    }
    starts
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
