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

/// Encode one document's forward record: `[dl][n terms][klen‖term…]`.
/// A tombstone reads this back to subtract the document from the
/// segment's corpus statistics — same numbers, exact withdrawal.
pub fn encode_fwd(dl: u32, terms: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, dl);
    put_varint(&mut out, terms.len() as u32);
    for t in terms {
        put_varint(&mut out, t.len() as u32);
        out.extend_from_slice(t);
    }
    out
}

/// Decode a forward record. `None` on any malformed frame.
pub fn decode_fwd(payload: &[u8]) -> Option<(u32, Vec<Vec<u8>>)> {
    let mut at = 0usize;
    let dl = read_varint(payload, &mut at)?;
    let n = read_varint(payload, &mut at)? as usize;
    let mut terms = Vec::with_capacity(n);
    for _ in 0..n {
        let klen = read_varint(payload, &mut at)? as usize;
        terms.push(payload.get(at..at + klen)?.to_vec());
        at += klen;
    }
    (at == payload.len()).then_some((dl, terms))
}

/// Decode a payload back to its entries. `None` on any malformed
/// frame — a corrupt payload is a refusal upstream, never a guess.
pub fn decode_posting(payload: &[u8]) -> Option<Vec<ColdEntry>> {
    let mut at = 0usize;
    let n = read_varint(payload, &mut at)? as usize;
    let mut out = Vec::with_capacity(n);
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
            fwd.insert(key.clone(), encode_fwd(dl, &doc_terms));
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
mod tests {
    use super::*;
    use crate::CorpusStats;

    fn stats(n_docs: f64, avgdl: f64, df: &[(&[u8], u32)]) -> CorpusStats {
        CorpusStats {
            n_docs,
            avgdl,
            df: df.iter().map(|(t, d)| (t.to_vec(), *d)).collect(),
        }
    }

    fn seg_with_docs() -> TextSegment {
        let mut ts = TextSegment::new();
        for (key, text) in [
            (b"ev:1".as_slice(), b"rust engine fast".as_slice()),
            (b"ev:2", b"rust storage engine"),
            (b"ev:3", b"slow python glue"),
            (b"ev:4", b"rust rust rust everywhere"),
        ] {
            ts.apply_doc(key, Some(&[(text.to_vec(), 1.0)]), &[]);
        }
        ts
    }

    #[test]
    fn codec_round_trips_and_refuses_garbage() {
        let entries = vec![
            ColdEntry { key: b"a\x00b".to_vec(), tf: 3, dl: 7, positions: vec![1, 2, 3] },
            ColdEntry { key: Vec::new(), tf: 1, dl: 1, positions: Vec::new() },
            ColdEntry { key: b"row:very-long-key-9999".to_vec(), tf: 200, dl: 4000, positions: vec![0; 40] },
        ];
        let payload = encode_posting(&entries);
        assert_eq!(posting_df(&payload), Some(3));
        let back = decode_posting(&payload).expect("decodes");
        assert_eq!(back.len(), 3);
        for (a, b) in entries.iter().zip(&back) {
            assert_eq!((&a.key, a.tf, a.dl, &a.positions), (&b.key, b.tf, b.dl, &b.positions));
        }
        assert!(decode_posting(&payload[..payload.len() - 1]).is_none(), "truncated");
        assert!(decode_posting(b"\xff\xff\xff\xff\xff").is_none(), "overlong varint");
    }

    #[test]
    fn frozen_scores_equal_hot_scores_under_the_same_stats() {
        let mut ts = seg_with_docs();
        let st = stats(4.0, 3.25, &[(b"rust", 3), (b"engine", 2)]);
        // Hot scores for the docs we are about to freeze.
        let hot: std::collections::HashMap<Vec<u8>, f64> = ts
            .matches_scored(b"rust engine", 10, Some(&st))
            .into_iter()
            .map(|m| (m.key, m.score))
            .collect();

        let bucket = ts
            .freeze_docs(&[b"ev:1".to_vec(), b"ev:4".to_vec()])
            .expect("froze");
        assert_eq!(bucket.n_docs, 2);
        let mut acc = std::collections::HashMap::new();
        for term in [b"rust".as_slice(), b"engine"] {
            if let Some(p) = bucket.terms.get(term) {
                score_cold(p, term, &st, &|_| false, &mut acc).expect("scores");
            }
        }
        for key in [b"ev:1".as_slice(), b"ev:4"] {
            let cold = acc.get(key).copied().expect("cold hit");
            let hot = hot.get(key).copied().expect("hot hit");
            assert!(
                (cold - hot).abs() < 1e-12,
                "score drifted for {:?}: hot {hot} vs cold {cold}",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn freeze_reclaims_and_hot_queries_stop_seeing_frozen_docs() {
        let mut ts = seg_with_docs();
        let before = ts.stats().approx_bytes;
        let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:3".to_vec(), b"ev:9".to_vec()]).expect("froze");
        assert_eq!(bucket.n_docs, 2, "unknown key skipped");
        assert!(ts.stats().approx_bytes < before, "nothing reclaimed");
        assert_eq!(ts.docs(), 2);
        let st = stats(4.0, 3.25, &[(b"rust", 3)]);
        let hot: Vec<_> = ts.matches_scored(b"rust", 10, Some(&st)).into_iter().map(|m| m.key).collect();
        assert!(!hot.contains(&b"ev:1".to_vec()), "frozen doc still hot");
        assert!(hot.contains(&b"ev:2".to_vec()));
        // The bucket's terms are ascending — the segment builder's contract.
        let ts_keys: Vec<_> = bucket.terms.keys().cloned().collect();
        let mut sorted = ts_keys.clone();
        sorted.sort();
        assert_eq!(ts_keys, sorted);
    }

    #[test]
    fn fwd_codec_round_trips_and_freeze_carries_exact_withdrawals() {
        let payload = encode_fwd(7, &[b"alpha".as_slice(), b"beta"]);
        assert_eq!(decode_fwd(&payload), Some((7, vec![b"alpha".to_vec(), b"beta".to_vec()])));
        assert!(decode_fwd(&payload[..payload.len() - 1]).is_none(), "truncated");

        let mut ts = seg_with_docs();
        let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:4".to_vec()]).expect("froze");
        // ev:1 "rust engine fast" (dl 3), ev:4 "rust rust rust everywhere" (dl 4).
        let (dl1, mut t1) = decode_fwd(bucket.fwd.get(b"ev:1".as_slice()).expect("fwd")).unwrap();
        t1.sort();
        assert_eq!((dl1, t1), (3, vec![b"engine".to_vec(), b"fast".to_vec(), b"rust".to_vec()]));
        let (dl4, t4) = decode_fwd(bucket.fwd.get(b"ev:4".as_slice()).expect("fwd")).unwrap();
        assert_eq!((dl4, t4), (4, vec![b"everywhere".to_vec(), b"rust".to_vec()]));
        // Withdrawing both restores an empty contribution exactly.
        assert_eq!(bucket.n_docs - 2, 0);
        assert_eq!(bucket.total_len - u64::from(dl1) - u64::from(dl4), 0);
    }

    #[test]
    fn tombstoned_rows_are_skipped() {
        let mut ts = seg_with_docs();
        let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:4".to_vec()]).expect("froze");
        let st = stats(4.0, 3.25, &[(b"rust", 3)]);
        let mut acc = std::collections::HashMap::new();
        let p = bucket.terms.get(b"rust".as_slice()).expect("term");
        score_cold(p, b"rust", &st, &|k| k == b"ev:4", &mut acc).expect("scores");
        assert!(acc.contains_key(b"ev:1".as_slice()));
        assert!(!acc.contains_key(b"ev:4".as_slice()), "tombstoned row scored");
    }
}
