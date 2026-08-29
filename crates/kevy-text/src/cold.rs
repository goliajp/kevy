//! The frozen half of a text index: the codec a cold bucket segment's
//! posting payloads use, the freeze that produces them, and the
//! scorer that reads them back — all pure (no I/O; the segment file
//! itself is the engine's concern).
//!
//! A frozen posting is keyed by row KEY, never by the hot segment's
//! recycled doc id, and scores against the same injected
//! [`crate::CorpusStats`] the hot two-pass query uses — which is what
//! makes a cold hit's score comparable to a hot hit's by construction.

use std::collections::{BTreeMap, HashMap};

use crate::bm25::bm25_score;
use crate::positions::walk;
use crate::docblobs::put_varint;
use crate::segment::TextSegment;

/// One slide batch's worth of frozen text entries: term → encoded
/// posting payload, in term order (the segment builder's key order),
/// plus the bucket's contribution to the corpus statistics.
pub struct FrozenBucket {
    /// term → [`encode_posting`] payload, ascending by term.
    pub terms: BTreeMap<Vec<u8>, Vec<u8>>,
    /// row key → [`encode_fwd`] payload, ascending by key — the
    /// forward records a later tombstone reads back to withdraw this
    /// document's statistics contribution exactly.
    pub fwd: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Documents frozen.
    pub n_docs: u64,
    /// Their summed (unweighted) token length.
    pub total_len: u64,
}

/// One decoded cold posting entry.
pub struct ColdEntry {
    /// The document's row key.
    pub key: Vec<u8>,
    /// Weighted term frequency.
    pub tf: u32,
    /// Document length (unweighted tokens).
    pub dl: u32,
    /// The positions blob, verbatim from the hot channel; empty when
    /// the index was not declared `WITH POSITIONS`.
    pub positions: Vec<u8>,
}

/// Encode one term's cold posting list:
/// `[n varint]` then per doc `[klen][key][tf][dl][plen][pos]`.
pub fn encode_posting(docs: &[ColdEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, docs.len() as u32);
    for d in docs {
        put_varint(&mut out, d.key.len() as u32);
        out.extend_from_slice(&d.key);
        put_varint(&mut out, d.tf);
        put_varint(&mut out, d.dl);
        put_varint(&mut out, d.positions.len() as u32);
        out.extend_from_slice(&d.positions);
    }
    out
}

/// The document frequency a payload carries — its header, no walk.
pub fn posting_df(payload: &[u8]) -> Option<u32> {
    read_varint(payload, &mut 0)
}

/// One decoded forward record: the document's length, its terms, and
/// its stored values (aligned with the declared VALUES order).
pub struct FwdRecord {
    /// Document length (unweighted tokens).
    pub dl: u32,
    /// Every term the document held, ascending.
    pub terms: Vec<Vec<u8>>,
    /// Stored values; `None` = the document has no value for the field
    /// (absent is not a value — a predicate never passes on it).
    pub values: Vec<Option<Vec<u8>>>,
}

/// Encode one document's forward record:
/// `[dl][n terms][klen‖term…][n values][per value: 0 | 1‖len‖bytes]`.
/// A tombstone reads it back to subtract the document from the
/// segment's corpus statistics — same numbers, exact withdrawal — and
/// the value-reading clauses (FILTER / SORT / DISTINCT / FACET) read
/// it to serve a cold hit without touching the row.
pub fn encode_fwd(dl: u32, terms: &[&[u8]], values: &[Option<&[u8]>]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, dl);
    put_varint(&mut out, terms.len() as u32);
    for t in terms {
        put_varint(&mut out, t.len() as u32);
        out.extend_from_slice(t);
    }
    put_varint(&mut out, values.len() as u32);
    for v in values {
        match v {
            None => put_varint(&mut out, 0),
            Some(b) => {
                put_varint(&mut out, 1);
                put_varint(&mut out, b.len() as u32);
                out.extend_from_slice(b);
            }
        }
    }
    out
}

/// Upper bound on the initial reservation for a count read out of a cold
/// payload — not a limit on the decode, which returns `None` the moment the
/// payload cannot supply an entry.
///
/// Every entry here costs at least one byte (a varint length, or a varint
/// tag), so a payload of `len` bytes cannot honour a claim past `len`.
/// `read_varint` returns u32, so an unbounded claim reserves up to 4.29e9
/// elements — about 103 GB for a `Vec<Vec<u8>>`. Fifth and sixth of the same
/// shape this release; the earlier ones came from fuzzers, these from
/// listing every allocation whose size comes out of the bytes.
pub(crate) fn entries_fit(n: usize, payload_len: usize) -> usize {
    n.min(payload_len)
}

/// Decode a forward record. `None` on any malformed frame.
pub fn decode_fwd(payload: &[u8]) -> Option<FwdRecord> {
    let mut at = 0usize;
    let dl = read_varint(payload, &mut at)?;
    let n = read_varint(payload, &mut at)? as usize;
    let mut terms = Vec::with_capacity(entries_fit(n, payload.len()));
    for _ in 0..n {
        let klen = read_varint(payload, &mut at)? as usize;
        terms.push(payload.get(at..at + klen)?.to_vec());
        at += klen;
    }
    let nv = read_varint(payload, &mut at)? as usize;
    let mut values = Vec::with_capacity(entries_fit(nv, payload.len()));
    for _ in 0..nv {
        values.push(match read_varint(payload, &mut at)? {
            0 => None,
            1 => {
                let vlen = read_varint(payload, &mut at)? as usize;
                let v = payload.get(at..at + vlen)?.to_vec();
                at += vlen;
                Some(v)
            }
            _ => return None,
        });
    }
    (at == payload.len()).then_some(FwdRecord { dl, terms, values })
}

/// Decode a payload back to its entries. `None` on any malformed
/// frame — a corrupt payload is a refusal upstream, never a guess.
pub fn decode_posting(payload: &[u8]) -> Option<Vec<ColdEntry>> {
    let mut at = 0usize;
    let n = read_varint(payload, &mut at)? as usize;
    let mut out = Vec::with_capacity(entries_fit(n, payload.len()));
    for _ in 0..n {
        let klen = read_varint(payload, &mut at)? as usize;
        let key = payload.get(at..at + klen)?.to_vec();
        at += klen;
        let tf = read_varint(payload, &mut at)?;
        let dl = read_varint(payload, &mut at)?;
        let plen = read_varint(payload, &mut at)? as usize;
        let positions = payload.get(at..at + plen)?.to_vec();
        at += plen;
        out.push(ColdEntry { key, tf, dl, positions });
    }
    (at == payload.len()).then_some(out)
}

/// Accumulate one term's cold contributions into `acc` under the
/// injected corpus statistics — the same formula, the same globals,
/// the same scale as the hot path. `dead` shadows revived/deleted
/// rows; no MaxScore pruning (a hot-only threshold would LOSE cold
/// documents, not merely misrank them).
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
        .map(|p| {
            decode_posting(p).map(|es| es.into_iter().map(|e| (e.key.clone(), e)).collect())
        })
        .collect::<Option<_>>()?;
    let distinct = crate::segment::distinct_tokens(toks);
    for (key, first) in &per_tok[0] {
        if dead(key) || !per_tok[1..].iter().all(|m| m.contains_key(key)) {
            continue;
        }
        let adjacent = walk(&first.positions).any(|start| {
            toks.iter().enumerate().skip(1).all(|(i, _)| {
                walk(&per_tok[i][key].positions).any(|p| p == start + i as u32)
            })
        });
        if !adjacent {
            continue;
        }
        let dl = f64::from(first.dl);
        let mut score = 0.0;
        for t in &distinct {
            let Some(pos) = toks.iter().position(|tt| tt == t) else { continue };
            let e = &per_tok[pos][key];
            let df = f64::from(
                *stats.df.get(t).unwrap_or(&(per_tok[pos].len() as u32)),
            );
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
pub fn highlight_fields(
    fields: &[Vec<u8>],
    query: &[u8],
) -> Vec<(usize, Vec<(usize, usize)>)> {
    let (bare, phrases, prefixes) = crate::parse_clauses(query);
    let terms: std::collections::HashSet<&[u8]> = bare.iter().map(Vec::as_slice).collect();
    let mut out = Vec::new();
    for (fi, text) in fields.iter().enumerate() {
        let mut spans = crate::segment::field_spans(
            &crate::tokenize_spans(text),
            &terms,
            &phrases,
            &prefixes,
        );
        if !spans.is_empty() {
            spans.sort_unstable();
            spans.dedup();
            out.push((fi, spans));
        }
    }
    out
}

fn read_varint(b: &[u8], at: &mut usize) -> Option<u32> {
    let mut cur = 0u32;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*at)?;
        *at += 1;
        cur |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(cur);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

impl TextSegment {
    /// Freeze `keys` out of the hot index: read each document's terms,
    /// term frequencies and positions blobs FIRST (withdraw consumes
    /// the stored source text they are derived from), then withdraw —
    /// reclaiming the doc record, its postings slots and its positions
    /// in one motion. Keys not indexed are skipped. `None` when
    /// nothing froze.
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

#[cfg(test)]
mod bound_tests {
    /// A count out of a cold payload cannot size an allocation.
    ///
    /// Sixth site of this shape in one release. The first three were found
    /// by fuzzers pointing at them, which is why the last three were found
    /// by listing every allocation whose size comes out of the bytes instead
    /// of waiting for the next crash.
    #[test]
    fn a_count_from_a_payload_cannot_size_an_allocation() {
        use super::entries_fit;
        assert_eq!(entries_fit(3, 1024), 3, "an honest count is used as-is");
        assert_eq!(
            entries_fit(u32::MAX as usize, 40),
            40,
            "4.29e9 entries over forty bytes reserves the ceiling, not 103 GB"
        );
        // One byte per entry is the floor, so a payload can always honour
        // `len` of them — an honest payload is never short-reserved.
        for len in [0usize, 1, 64, 4096] {
            assert_eq!(entries_fit(len, len), len, "len at {len} still fits exactly");
        }

        // The decode refuses the lie either way, which is why the assertion
        // that sees this defect is the one above and not this one.
        let mut payload = vec![0xffu8, 0xff, 0xff, 0xff, 0x0f]; // varint u32::MAX
        payload.push(0x00);
        assert!(super::decode_posting(&payload).is_none());
    }
}
