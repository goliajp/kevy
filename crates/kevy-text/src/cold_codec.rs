//! The cold payload codec: posting lists keyed by row key, and the
//! per-document forward records a tombstone reads back.

use crate::docblobs::{next_varint as read_varint, put_varint};

/// One cold posting entry: what [`encode_posting`] takes and
/// [`decode_posting`] gives back.
///
/// ```
/// use kevy_text::cold::{ColdEntry, decode_posting, encode_posting};
/// let mut e = ColdEntry::default();
/// e.key = b"doc:1".to_vec();
/// e.tf = 2;
/// e.dl = 7;
/// let back = decode_posting(&encode_posting(&[e.clone()])).ok_or("malformed")?;
/// assert_eq!(back, vec![e]);
/// # Ok::<(), &str>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ColdEntry {
    /// The document's row key.
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_posting};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let entries = decode_posting(&bucket.terms[&b"rust".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(entries[0].key, b"doc:1");
    /// # Ok::<(), &str>(())
    /// ```
    pub key: Vec<u8>,
    /// Weighted term frequency.
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_posting};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust loves rust"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let entries = decode_posting(&bucket.terms[&b"rust".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(entries[0].tf, 2, "the term occurs twice");
    /// # Ok::<(), &str>(())
    /// ```
    pub tf: u32,
    /// Document length (unweighted tokens).
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_posting};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust storage engine"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let entries = decode_posting(&bucket.terms[&b"rust".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(entries[0].dl, 3);
    /// # Ok::<(), &str>(())
    /// ```
    pub dl: u32,
    /// The positions blob, verbatim from the hot channel; empty when
    /// the index was not declared `WITH POSITIONS`.
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_posting};
    /// let first_positions = |mut seg: TextSegment| {
    ///     seg.apply(b"doc:1", Some(b"rust engine"));
    ///     let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).expect("froze");
    ///     decode_posting(&bucket.terms[&b"rust".to_vec()]).expect("well formed")[0]
    ///         .positions
    ///         .clone()
    /// };
    /// assert!(first_positions(TextSegment::new()).is_empty());
    /// assert!(!first_positions(TextSegment::with_positions()).is_empty());
    /// ```
    pub positions: Vec<u8>,
}

/// Encode one term's cold posting list:
/// `[n varint]` then per doc `[klen][key][tf][dl][plen][pos]`.
///
/// ```
/// use kevy_text::cold::{ColdEntry, decode_posting, encode_posting, posting_df};
/// let mut a = ColdEntry::default();
/// a.key = b"doc:1".to_vec();
/// a.tf = 1;
/// a.dl = 4;
/// let mut b = a.clone();
/// b.key = b"doc:2".to_vec();
/// let payload = encode_posting(&[a.clone(), b.clone()]);
/// assert_eq!(posting_df(&payload), Some(2));
/// assert_eq!(decode_posting(&payload), Some(vec![a, b]));
/// ```
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
///
/// ```
/// use kevy_text::{TextSegment, cold::posting_df};
/// let mut seg = TextSegment::new();
/// seg.apply(b"doc:1", Some(b"rust engine"));
/// seg.apply(b"doc:2", Some(b"rust client"));
/// let bucket = seg.freeze_docs(&[b"doc:1".to_vec(), b"doc:2".to_vec()]).ok_or("nothing froze")?;
/// assert_eq!(posting_df(&bucket.terms[&b"rust".to_vec()]), Some(2));
/// assert_eq!(posting_df(&bucket.terms[&b"engine".to_vec()]), Some(1));
/// assert_eq!(posting_df(&[]), None, "an empty payload has no header");
/// # Ok::<(), &str>(())
/// ```
pub fn posting_df(payload: &[u8]) -> Option<u32> {
    read_varint(payload, &mut 0)
}

/// One decoded forward record: the document's length, its terms, and
/// its stored values (aligned with the declared VALUES order).
///
/// ```
/// use kevy_text::cold::{decode_fwd, encode_fwd};
/// let rec = decode_fwd(&encode_fwd(2, &[b"engine", b"rust"], &[Some(b"42")])).ok_or("malformed")?;
/// assert_eq!(rec.dl, 2);
/// assert_eq!(rec.terms, vec![b"engine".to_vec(), b"rust".to_vec()]);
/// assert_eq!(rec.values, vec![Some(b"42".to_vec())]);
/// # Ok::<(), &str>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct FwdRecord {
    /// Document length (unweighted tokens).
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_fwd};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"small rust storage engine"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let rec = decode_fwd(&bucket.fwd[&b"doc:1".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(rec.dl, 4);
    /// # Ok::<(), &str>(())
    /// ```
    pub dl: u32,
    /// Every term the document held, ascending.
    ///
    /// ```
    /// use kevy_text::{TextSegment, cold::decode_fwd};
    /// let mut seg = TextSegment::new();
    /// seg.apply(b"doc:1", Some(b"rust engine rust"));
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let rec = decode_fwd(&bucket.fwd[&b"doc:1".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(rec.terms, vec![b"engine".to_vec(), b"rust".to_vec()]);
    /// # Ok::<(), &str>(())
    /// ```
    pub terms: Vec<Vec<u8>>,
    /// Stored values; `None` = the document has no value for the field
    /// (absent is not a value — a predicate never passes on it).
    ///
    /// ```
    /// use kevy_text::{SegmentShape, TextSegment, cold::decode_fwd};
    /// let mut seg = TextSegment::with_shape(SegmentShape::default().with_values(2));
    /// seg.apply_doc(b"doc:1", Some(&[(b"rust".to_vec(), 1.0)]), &[Some(b"9.5"), None]);
    /// let bucket = seg.freeze_docs(&[b"doc:1".to_vec()]).ok_or("nothing froze")?;
    /// let rec = decode_fwd(&bucket.fwd[&b"doc:1".to_vec()]).ok_or("malformed")?;
    /// assert_eq!(rec.values, vec![Some(b"9.5".to_vec()), None]);
    /// # Ok::<(), &str>(())
    /// ```
    pub values: Vec<Option<Vec<u8>>>,
}

/// Encode one document's forward record:
/// `[dl][n terms][klen‖term…][n values][per value: 0 | 1‖len‖bytes]`.
/// A tombstone reads it back to subtract the document from the
/// segment's corpus statistics — same numbers, exact withdrawal — and
/// the value-reading clauses (FILTER / SORT / DISTINCT / FACET) read
/// it to serve a cold hit without touching the row.
///
/// ```
/// use kevy_text::cold::{decode_fwd, encode_fwd};
/// let payload = encode_fwd(3, &[b"rust"], &[None, Some(b"tokyo")]);
/// let rec = decode_fwd(&payload).ok_or("malformed")?;
/// assert_eq!((rec.dl, rec.terms.len()), (3, 1));
/// assert_eq!(rec.values, vec![None, Some(b"tokyo".to_vec())]);
/// # Ok::<(), &str>(())
/// ```
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
///
/// ```
/// use kevy_text::cold::{decode_fwd, encode_fwd};
/// let payload = encode_fwd(1, &[b"rust"], &[]);
/// assert_eq!(decode_fwd(&payload).map(|r| r.dl), Some(1));
/// assert!(decode_fwd(&payload[..payload.len() - 1]).is_none(), "truncated");
/// let mut trailing = payload.clone();
/// trailing.push(0);
/// assert!(decode_fwd(&trailing).is_none(), "trailing bytes");
/// ```
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
///
/// ```
/// use kevy_text::{TextSegment, cold::decode_posting};
/// let mut seg = TextSegment::new();
/// seg.apply(b"doc:1", Some(b"rust"));
/// seg.apply(b"doc:2", Some(b"rust rust"));
/// let bucket = seg.freeze_docs(&[b"doc:1".to_vec(), b"doc:2".to_vec()]).ok_or("nothing froze")?;
/// let payload = &bucket.terms[&b"rust".to_vec()];
/// let entries = decode_posting(payload).ok_or("malformed")?;
/// let tfs: Vec<(&[u8], u32)> = entries.iter().map(|e| (e.key.as_slice(), e.tf)).collect();
/// assert_eq!(tfs, vec![(&b"doc:1"[..], 1), (&b"doc:2"[..], 2)]);
/// assert!(decode_posting(&payload[..payload.len() - 1]).is_none(), "truncated");
/// # Ok::<(), &str>(())
/// ```
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
