//! What one query clause contributes to a document's score — the
//! accumulation half of the query engine, shared by the whole-document
//! path and the field-scoped (`IN <field…>`) one. A child module of
//! `segment` (declared via `#[path]`), so it reaches `TextSegment`'s
//! private fields.
//!
//! Both paths run the *same* clause engine; only where a frequency and a
//! length come from differs. Unscoped, they come from the merged
//! postings, exactly as before this module existed. Scoped, they come
//! from the per-field channel ([`crate::fields`]): the term frequency is
//! the sum over the wanted fields only, the length is those fields'
//! length only, and the document frequency counts documents holding the
//! term *in those fields* — a real field-scoped BM25, not a filter over
//! whole-document scores.

use std::collections::HashMap;

use super::{CorpusStats, TextSegment};
use crate::bm25::bm25_score;

/// The context a clause accumulates against: the corpus statistics to
/// score with, and which fields the query is restricted to.
#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    pub(crate) stats: Option<&'a CorpusStats>,
    pub(crate) n_docs: f64,
    pub(crate) avgdl: f64,
    /// Field positions the query is restricted to (`IN`); empty = the
    /// whole document, scored from the merged postings.
    pub(crate) want: &'a [usize],
}

impl Scope<'_> {
    /// Whether this query is restricted to a subset of the fields.
    pub(crate) fn scoped(&self) -> bool {
        !self.want.is_empty()
    }

    /// `t`'s document frequency: the corpus-wide value when the query
    /// carries one (the cross-shard second pass), else this segment's own
    /// count.
    fn df(&self, t: &[u8], local: usize) -> f64 {
        self.stats.and_then(|s| s.df.get(t)).map(|&d| f64::from(d)).unwrap_or(local as f64)
    }
}

impl TextSegment {
    /// The corpus size and average document length to score with, over
    /// the scope's fields.
    ///
    /// Injected statistics are used as given: the first pass computed
    /// them over the same field scope, so a scoped query's `avgdl` is
    /// already the average length *of those fields*.
    pub(crate) fn scope_stats(&self, stats: Option<&CorpusStats>, want: &[usize]) -> (f64, f64) {
        if want.is_empty() || stats.is_some() {
            return self.corpus_stats(stats);
        }
        let n = self.docs.len() as f64;
        (n, self.total_len_in(want) as f64 / n)
    }

    /// Corpus token total over `fields` — the numerator of a field-scoped
    /// average document length, and what each shard reports in the first
    /// pass of a scoped cross-shard query. Empty `fields` = the whole
    /// document.
    pub fn total_len_in(&self, fields: &[usize]) -> u64 {
        match self.fields.as_ref() {
            Some(fs) if !fields.is_empty() => fs.total_len_in(fields),
            _ => self.total_len,
        }
    }

    /// Add one term's contribution with typo tolerance: the OR of every
    /// dictionary term within `budget` edits. With `budget` 0 this is
    /// exactly [`Self::add_term`], so the exact path stays untouched.
    pub(crate) fn add_typo(
        &self,
        t: &[u8],
        budget: u32,
        scores: &mut HashMap<u32, f64>,
        sc: &Scope,
    ) {
        if budget == 0 {
            self.add_term(t, scores, sc);
            return;
        }
        for cand in self.expand_typo(t, budget) {
            self.add_term(cand, scores, sc);
        }
    }

    /// Add one prefix clause's contribution: the OR of every expansion
    /// term (already-lowercased `pfx` matched against the stored token
    /// form). Scanning the dictionary is the cost an ordered dictionary
    /// would replace with a binary search.
    pub(crate) fn add_prefix(&self, pfx: &[u8], scores: &mut HashMap<u32, f64>, sc: &Scope) {
        for t in self.expand_prefix(pfx) {
            self.add_term(t, scores, sc);
        }
    }

    /// Add one bare term's BM25 contribution to every document that holds
    /// it — a full-list walk (no MaxScore pruning), used only on the
    /// phrase path where a phrase clause could otherwise boost a document
    /// pruning would have dropped.
    pub(crate) fn add_term(&self, t: &[u8], scores: &mut HashMap<u32, f64>, sc: &Scope) {
        if sc.scoped() {
            self.add_term_scoped(t, scores, sc);
            return;
        }
        let Some(list) = self.postings.get(t) else { return };
        let df = sc.df(t, list.len());
        for (tf, bands) in list.tf_groups() {
            for (_b, band) in bands.iter() {
                for &id in band {
                    let dl = f64::from(self.id_dl[id as usize]);
                    *scores.entry(id).or_insert(0.0) +=
                        bm25_score(f64::from(tf), df, sc.n_docs, dl, sc.avgdl);
                }
            }
        }
    }

    /// The field-scoped counterpart: walk the per-field channel, keeping
    /// documents that hold `t` in one of the wanted fields and scoring
    /// each by those fields' frequency and length alone.
    fn add_term_scoped(&self, t: &[u8], scores: &mut HashMap<u32, f64>, sc: &Scope) {
        let Some(fs) = self.fields.as_ref() else { return };
        let docs = fs.docs_in(t, sc.want);
        let df = sc.df(t, docs.len());
        for (id, tf) in docs {
            let dl = f64::from(fs.doc_len_in(id, sc.want));
            *scores.entry(id).or_insert(0.0) +=
                bm25_score(f64::from(tf), df, sc.n_docs, dl, sc.avgdl);
        }
    }

    /// One document's summed BM25 over a set of distinct tokens — what an
    /// AND query over them would score it, and so what a phrase
    /// occurrence is worth.
    pub(crate) fn clause_score(&self, distinct: &[Vec<u8>], id: u32, sc: &Scope) -> f64 {
        if sc.scoped() {
            return self.clause_score_scoped(distinct, id, sc);
        }
        let dl = f64::from(self.id_dl[id as usize]);
        let mut score = 0.0;
        for t in distinct {
            let Some(list) = self.postings.get(t) else { continue };
            let Some(tf) = list.get(id) else { continue };
            score += bm25_score(f64::from(tf), sc.df(t, list.len()), sc.n_docs, dl, sc.avgdl);
        }
        score
    }

    /// The field-scoped counterpart of [`Self::clause_score`].
    fn clause_score_scoped(&self, distinct: &[Vec<u8>], id: u32, sc: &Scope) -> f64 {
        let Some(fs) = self.fields.as_ref() else { return 0.0 };
        let dl = f64::from(fs.doc_len_in(id, sc.want));
        let mut score = 0.0;
        for t in distinct {
            let docs = fs.docs_in(t, sc.want);
            let Some(&(_, tf)) = docs.iter().find(|(d, _)| *d == id) else { continue };
            score += bm25_score(f64::from(tf), sc.df(t, docs.len()), sc.n_docs, dl, sc.avgdl);
        }
        score
    }

    /// The dictionary terms beginning with `pfx` (already lowercased),
    /// sorted for a deterministic ranking tiebreak.
    ///
    /// The first byte is checked inline before `starts_with`, which for a
    /// runtime-length needle is a `memcmp` **call** per term. This scan
    /// visits the whole dictionary, so that call is made once per indexed
    /// term per prefix clause, and a profile of the prefix workload puts
    /// 55% of query time inside `__memcmp_avx2_movbe`. One byte rejects
    /// nearly all of them without the call.
    pub(crate) fn expand_prefix(&self, pfx: &[u8]) -> Vec<&[u8]> {
        // An empty prefix matches everything, which is what `starts_with`
        // did and what the head-byte filter would not: callers never send
        // one, but the shortcut must not be where that stops being true.
        let mut e: Vec<&[u8]> = match pfx.first() {
            None => self.postings.keys().map(Vec::as_slice).collect(),
            Some(&head) => self
                .postings
                .keys()
                .map(Vec::as_slice)
                .filter(|t| t.first() == Some(&head) && t.starts_with(pfx))
                .collect(),
        };
        e.sort_unstable();
        e
    }

    /// The dictionary terms within `budget` edits of `t` — the typo
    /// tolerance expansion, including `t` itself when it is indexed.
    /// Sorted for a deterministic ranking tiebreak.
    pub(crate) fn expand_typo(&self, t: &[u8], budget: u32) -> Vec<&[u8]> {
        let mut e: Vec<&[u8]> = self
            .postings
            .keys()
            .map(Vec::as_slice)
            .filter(|cand| crate::edit::edit_within(t, cand, budget).is_some())
            .collect();
        e.sort_unstable();
        e
    }

    /// The phrase token with the fewest postings — the tightest candidate
    /// set, since every phrase token must appear in a matching document,
    /// so anchoring the scan on the rarest one avoids walking a head
    /// term's whole list. `None` if any token is absent (the phrase then
    /// matches nothing).
    pub(crate) fn rarest_anchor<'a>(&self, toks: &'a [Vec<u8>]) -> Option<&'a [u8]> {
        let mut best: Option<(&'a [u8], usize)> = None;
        for t in toks {
            let df = self.postings.get(t)?.len();
            if best.is_none_or(|(_, b)| df < b) {
                best = Some((t, df));
            }
        }
        best.map(|(t, _)| t)
    }

    /// Whether a phrase occupying `[start, start + len)` of `id`'s token
    /// stream lies entirely inside one of the scope's fields.
    ///
    /// Positions are ordinals over the document's concatenated fields, so
    /// a field is a contiguous range of them: a scoped phrase must fall
    /// within a single wanted field's range, which also rejects a
    /// "phrase" that only reads as adjacent because one field's last
    /// token abuts the next field's first.
    pub(crate) fn phrase_in_scope(&self, id: u32, start: u32, len: u32, want: &[usize]) -> bool {
        let Some(fs) = self.fields.as_ref() else { return false };
        let lens = fs.doc_lens(id);
        let mut base = 0u32;
        for (f, &n) in lens.iter().enumerate() {
            if want.contains(&f) && start >= base && start + len <= base + n {
                return true;
            }
            base += n;
        }
        false
    }
}
