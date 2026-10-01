//! The frozen half of a text index: the codec a cold bucket segment's
//! posting payloads use, the freeze that produces them, and the
//! scorer that reads them back — all pure (no I/O; the segment file
//! itself is the engine's concern).
//!
//! A frozen posting is keyed by row KEY, never by the hot segment's
//! recycled doc id, and scores against the same injected
//! [`crate::CorpusStats`] the hot two-pass query uses — which is what
//! makes a cold hit's score comparable to a hot hit's by construction.
//!
//! ```
//! use kevy_text::{CorpusStats, TextSegment, cold::score_cold};
//! use std::collections::HashMap;
//! let mut hot = TextSegment::new();
//! hot.apply(b"doc:1", Some(b"rust engine"));
//! hot.apply(b"doc:2", Some(b"rust client"));
//! let stats = CorpusStats::new(2.0, 2.0, HashMap::from([(b"rust".to_vec(), 2)]));
//! let hot_score = hot.matches_scored(b"rust", 1, Some(&stats))[0].score;
//!
//! let bucket = hot.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
//! let mut acc = HashMap::new();
//! score_cold(&bucket.terms[&b"rust".to_vec()], b"rust", &stats, &|_| false, &mut acc)
//!     .ok_or("malformed")?;
//! // The same document scores the same whether it is hot or frozen.
//! assert!((acc[&b"doc:1".to_vec()] - hot_score).abs() < 1e-9);
//! # Ok::<(), &str>(())
//! ```

use std::collections::{BTreeMap, HashMap};

use crate::bm25::bm25_score;
use crate::positions::walk;
use crate::segment::TextSegment;

#[path = "cold_codec.rs"]
mod cold_codec;
pub use cold_codec::{
    ColdEntry, FwdRecord, decode_fwd, decode_posting, encode_fwd, encode_posting, posting_df,
};

/// One slide batch's worth of frozen text entries: term → encoded
/// posting payload, in term order (the segment builder's key order),
/// plus the bucket's contribution to the corpus statistics.
///
/// ```
/// use kevy_text::TextSegment;
/// let mut seg = TextSegment::new();
/// seg.apply(b"doc:1", Some(b"rust engine"));
/// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
/// assert_eq!(bucket.n_docs, 1);
/// assert_eq!(seg.stats().docs, 0, "the freeze withdrew it from the hot index");
/// # Ok::<(), &str>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct FrozenBucket {
    /// term → [`encode_posting`] payload, ascending by term.
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::posting_df};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// seg.apply(b"doc:2", Some(b"rust client"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec(), b"doc:2".to_vec()]).ok_or("nothing froze")?;
    /// let terms: Vec<&[u8]> = bucket.terms.keys().map(Vec::as_slice).collect();
    /// assert_eq!(terms, vec![&b"client"[..], b"engine", b"rust"]);
    /// assert_eq!(posting_df(&bucket.terms[&b"rust".to_vec()]), Some(2));
    /// # Ok::<(), &str>(())
    /// ```
    pub terms: BTreeMap<Vec<u8>, Vec<u8>>,
    /// row key → [`encode_fwd`] payload, ascending by key — the
    /// forward records a later tombstone reads back to withdraw this
    /// document's statistics contribution exactly.
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_fwd};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let rec = decode_fwd(&bucket.fwd[&b"doc:1".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(rec.dl, 2);
    /// assert_eq!(rec.terms, vec![b"engine".to_vec(), b"rust".to_vec()]);
    /// # Ok::<(), &str>(())
    /// ```
    pub fwd: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Documents frozen.
    ///
    /// ```
    /// use kevy_text::TextSegment;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust"));
    /// seg.apply(b"doc:2", Some(b"engine"));
    /// let keys = [b"doc:1".to_vec(), b"doc:2".to_vec(), b"never-indexed".to_vec()];
    /// let bucket = seg.freeze_docs(&keys).ok_or("nothing froze")?;
    /// assert_eq!(bucket.n_docs, 2, "a key that was never indexed is skipped");
    /// # Ok::<(), &str>(())
    /// ```
    pub n_docs: u64,
    /// Their summed (unweighted) token length.
    ///
    /// ```
    /// use kevy_text::TextSegment;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// seg.apply(b"doc:2", Some(b"fast rust client"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec(), b"doc:2".to_vec()]).ok_or("nothing froze")?;
    /// assert_eq!(bucket.total_len, 5);
    /// # Ok::<(), &str>(())
    /// ```
    pub total_len: u64,
}

/// Accumulate one term's cold contributions into `acc` under the
/// injected corpus statistics — the same formula, the same globals,
/// the same scale as the hot path. `dead` shadows revived/deleted
/// rows; no MaxScore pruning (a hot-only threshold would LOSE cold
/// documents, not merely misrank them).
///
/// ```
/// use kevy_text::{CorpusStats, TextSegment, cold::score_cold};
/// use std::collections::HashMap;
/// let mut seg = TextSegment::new();
/// seg.apply(b"doc:1", Some(b"rust rust engine"));
/// seg.apply(b"doc:2", Some(b"rust client library"));
/// let bucket = seg.freeze_docs(&[b"doc:1".to_vec(), b"doc:2".to_vec()]).ok_or("nothing froze")?;
/// let stats = CorpusStats::new(10.0, 3.0, HashMap::from([(b"rust".to_vec(), 2)]));
/// let payload = &bucket.terms[&b"rust".to_vec()];
///
/// let mut acc = HashMap::new();
/// score_cold(payload, b"rust", &stats, &|_| false, &mut acc).ok_or("malformed")?;
/// assert!(acc[&b"doc:1".to_vec()] > acc[&b"doc:2".to_vec()], "two hits beat one");
///
/// // A row deleted or revived since the freeze is shadowed.
/// let mut shadowed = HashMap::new();
/// score_cold(payload, b"rust", &stats, &|k| k == b"doc:1", &mut shadowed).ok_or("malformed")?;
/// assert_eq!(shadowed.len(), 1);
/// assert!(score_cold(b"\x05", b"rust", &stats, &|_| false, &mut acc).is_none(), "malformed");
/// # Ok::<(), &str>(())
/// ```
pub fn score_cold(
    payload: &[u8],
    term: &[u8],
    stats: &crate::CorpusStats,
    dead: &dyn Fn(&[u8]) -> bool,
    acc: &mut HashMap<Vec<u8>, f64>,
) -> Option<()> {
    let entries = decode_posting(payload)?;
    let df = f64::from(*stats.df.get(term).unwrap_or(&(entries.len() as u32)));
    for e in entries {
        if dead(&e.key) {
            continue;
        }
        let s = bm25_score(f64::from(e.tf), df, stats.n_docs, f64::from(e.dl), stats.avgdl);
        *acc.entry(e.key).or_insert(0.0) += s;
    }
    Some(())
}

/// Accumulate one phrase clause's cold contributions into `acc` —
/// the mirror of the hot `add_phrase`: a document scores the BM25 sum
/// of the phrase's DISTINCT tokens (what an AND query would give it),
/// once, iff the tokens occur consecutively and in order. `payloads`
/// aligns with `toks` (one term posting payload each; any token
/// absent from this segment = the phrase matches nothing here, the
/// `rarest_anchor` `None` mirror). Positions blobs travel verbatim
/// from the hot channel, so a segment frozen without `WITH POSITIONS`
/// has empty blobs and verifies nothing — exactly the hot refusal.
///
/// ```
/// use kevy_text::{CorpusStats, TextSegment, cold::score_cold_phrase};
/// use std::collections::HashMap;
/// let mut seg = TextSegment::with_positions();
/// seg.apply(b"doc:1", Some(b"rust storage engine"));
/// seg.apply(b"doc:2", Some(b"engine for rust storage"));
/// seg.apply(b"doc:3", Some(b"storage engine rust"));
/// let keys = [b"doc:1".to_vec(), b"doc:2".to_vec(), b"doc:3".to_vec()];
/// let bucket = seg.freeze_docs(&keys).ok_or("nothing froze")?;
/// let toks = vec![b"rust".to_vec(), b"storage".to_vec()];
/// let payloads: Vec<Vec<u8>> = toks.iter().map(|t| bucket.terms[t].clone()).collect();
/// let stats = CorpusStats::new(3.0, 3.0, HashMap::new());
///
/// let mut acc = HashMap::new();
/// score_cold_phrase(&payloads, &toks, &stats, &|_| false, &mut acc).ok_or("malformed")?;
/// let mut hits: Vec<&[u8]> = acc.keys().map(Vec::as_slice).collect();
/// hits.sort();
/// // doc:3 holds both tokens, but not as "rust storage".
/// assert_eq!(hits, vec![&b"doc:1"[..], b"doc:2"]);
/// # Ok::<(), &str>(())
/// ```
pub fn score_cold_phrase(
    payloads: &[Vec<u8>],
    toks: &[Vec<u8>],
    stats: &crate::CorpusStats,
    dead: &dyn Fn(&[u8]) -> bool,
    acc: &mut HashMap<Vec<u8>, f64>,
) -> Option<()> {
    if payloads.len() != toks.len() || toks.is_empty() {
        return None;
    }
    let per_tok: Vec<HashMap<Vec<u8>, ColdEntry>> = payloads
        .iter()
        .map(|p| decode_posting(p).map(|es| es.into_iter().map(|e| (e.key.clone(), e)).collect()))
        .collect::<Option<_>>()?;
    let distinct = crate::segment::distinct_tokens(toks);
    for (key, first) in &per_tok[0] {
        if dead(key) || !per_tok[1..].iter().all(|m| m.contains_key(key)) {
            continue;
        }
        let adjacent = walk(&first.positions).any(|start| {
            toks.iter()
                .enumerate()
                .skip(1)
                .all(|(i, _)| walk(&per_tok[i][key].positions).any(|p| p == start + i as u32))
        });
        if !adjacent {
            continue;
        }
        let dl = f64::from(first.dl);
        let mut score = 0.0;
        for t in &distinct {
            let Some(pos) = toks.iter().position(|tt| tt == t) else { continue };
            let e = &per_tok[pos][key];
            let df = f64::from(*stats.df.get(t).unwrap_or(&(per_tok[pos].len() as u32)));
            score += bm25_score(f64::from(e.tf), df, stats.n_docs, dl, stats.avgdl);
        }
        *acc.entry(key.clone()).or_insert(0.0) += score;
    }
    Some(())
}

/// Highlight spans over a document's raw field texts — the cold twin
/// of the hot `highlight_spans`, for a hit whose source text lives in
/// the ROW rather than the segment (the freeze consumed the stored
/// copy). Same re-analysis, same span rules, byte-identical output
/// for the same texts.
///
/// ```
/// use kevy_text::cold::highlight_fields;
/// let fields = vec![b"Kevy docs".to_vec(), b"a rust engine in rust".to_vec()];
/// let spans = highlight_fields(&fields, b"rust");
/// // Field 1 only, with a byte span per occurrence.
/// assert_eq!(spans, vec![(1, vec![(2, 6), (17, 21)])]);
/// assert_eq!(&fields[1][2..6], b"rust");
/// ```
pub fn highlight_fields(fields: &[Vec<u8>], query: &[u8]) -> Vec<(usize, Vec<(usize, usize)>)> {
    let (bare, phrases, prefixes) = crate::parse_clauses(query);
    let terms: std::collections::HashSet<&[u8]> = bare.iter().map(Vec::as_slice).collect();
    let mut out = Vec::new();
    for (fi, text) in fields.iter().enumerate() {
        let mut spans =
            crate::segment::field_spans(&crate::tokenize_spans(text), &terms, &phrases, &prefixes);
        if !spans.is_empty() {
            spans.sort_unstable();
            spans.dedup();
            out.push((fi, spans));
        }
    }
    out
}

impl TextSegment {
    /// Freeze `keys` out of the hot index: read each document's terms,
    /// term frequencies and positions blobs FIRST (withdraw consumes
    /// the stored source text they are derived from), then withdraw —
    /// reclaiming the doc record, its postings slots and its positions
    /// in one motion. Keys not indexed are skipped. `None` when
    /// nothing froze.
    ///
    /// ```
    /// use kevy_text::TextSegment;
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine"));
    /// seg.apply(b"doc:2", Some(b"rust client"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// assert_eq!(bucket.n_docs, 1);
    /// let left: Vec<Vec<u8>> = seg.matches(b"rust", 10).into_iter().map(|m| m.key).collect();
    /// assert_eq!(left, vec![b"doc:2".to_vec()], "doc:1 now lives only in the bucket");
    /// assert!(seg.freeze_docs(&[b"doc:1".to_vec()]).is_none(), "already frozen");
    /// # Ok::<(), &str>(())
    /// ```
    pub fn freeze_docs(&mut self, keys: &[Vec<u8>]) -> Option<FrozenBucket> {
        let mut terms: BTreeMap<Vec<u8>, Vec<ColdEntry>> = BTreeMap::new();
        let mut fwd: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut n_docs = 0u64;
        let mut total_len = 0u64;
        for key in keys {
            let Some((id, dl, tf_map)) = self.doc_terms(key) else { continue };
            n_docs += 1;
            total_len += u64::from(dl);
            let mut doc_terms: Vec<&[u8]> = tf_map.keys().map(Vec::as_slice).collect();
            doc_terms.sort_unstable();
            let values = self.doc_values_of(id);
            fwd.insert(key.clone(), encode_fwd(dl, &doc_terms, &values));
            for (t, tf) in &tf_map {
                let positions = self.positions_blob(t, id).map(<[u8]>::to_vec).unwrap_or_default();
                terms.entry(t.clone()).or_default().push(ColdEntry {
                    key: key.clone(),
                    tf: *tf,
                    dl,
                    positions,
                });
            }
        }
        if n_docs == 0 {
            return None;
        }
        // Withdraw is a safe no-op for keys that were never indexed.
        for key in keys {
            self.apply_doc(key, None, &[]);
        }
        let terms = terms
            .into_iter()
            .map(|(t, entries)| {
                let payload = encode_posting(&entries);
                (t, payload)
            })
            .collect();
        Some(FrozenBucket { terms, fwd, n_docs, total_len })
    }
}

#[cfg(test)]
#[path = "cold_tests.rs"]
mod tests;
