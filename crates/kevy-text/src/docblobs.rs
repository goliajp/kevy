//! Per-token, per-document byte blobs — the storage shape every physical
//! side-channel to the BM25 postings shares.
//!
//! Two side-channels hang off a segment: [`crate::positions`] (where in a
//! document each term occurred) and [`crate::fields`] (how often it
//! occurred in each field). Neither is on the ranking hot path, and both
//! want the same thing: for one token, a small blob per document, cheap
//! when the token is a hapax. So the shape lives here once — a `One`
//! variant that keeps a singleton token's blob inline (mirroring
//! [`crate::buckets::Buckets::One`]) and a `Many` map from the second
//! document on — and each channel supplies its own blob encoding.
//!
//! The LEB128 varint helpers are shared for the same reason: both
//! channels store small ascending or small-magnitude integers per token,
//! and a fixed 4 bytes apiece would dominate the blob.

use std::collections::HashMap;

/// LEB128-encode one value onto `out`.
pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// The last shift a `u32` varint can legally take: 32 bits at 7 a byte
/// leaves the fifth byte carrying bits 28..32, so a sixth continuation byte
/// is by definition an overlong encoding.
const VARINT_MAX_SHIFT: u32 = 28;

/// Decode one varint at `at`, advancing it past what was consumed.
///
/// `None` for the two ways a stream can stop having a next value: it ended,
/// or it ended mid-varint (a trailing continuation byte with nothing after
/// it), or the varint is overlong.
///
/// **Why the shift is bounded.** `put_varint` cannot emit more than five
/// bytes, so a sixth continuation byte cannot come from this encoder and the
/// bound is unreachable by argument. It is here because "unreachable by
/// argument" and "does something sensible if reached" have to both be true:
/// without it, `1 << 35` panics in a debug build (`attempt to shift left
/// with overflow`) and silently masks to `1 << 3` in a release one. Six
/// bytes — `80 80 80 80 80 01` — was enough to get both. Two profiles giving
/// two different wrong answers for the same input is the shape this refuses.
pub(crate) fn next_varint(blob: &[u8], at: &mut usize) -> Option<u32> {
    let mut cur = 0u32;
    let mut shift = 0u32;
    loop {
        let b = *blob.get(*at)?;
        *at += 1;
        cur |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(cur);
        }
        shift += 7;
        if shift > VARINT_MAX_SHIFT {
            return None;
        }
    }
}

/// Decode a varint stream back to the values that were pushed.
///
/// Stops at the first byte that is not part of a whole, well-formed varint.
/// For a blob this crate wrote that is the end of the blob.
pub(crate) fn get_varints(blob: &[u8]) -> Vec<u32> {
    let mut at = 0;
    let mut out = Vec::new();
    while let Some(v) = next_varint(blob, &mut at) {
        out.push(v);
    }
    out
}

/// One token's per-document blobs. A hapax token — a unique id / email /
/// doc number that appears in exactly one document, the common Zipf case
/// — keeps its blob inline; the map only materializes from the second
/// document on. Mirrors [`crate::buckets::Buckets::One`], so a singleton
/// token pays no HashMap allocation (the "+2GiB over 1M singletons" shape
/// the impact buckets already avoid).
#[derive(Debug)]
pub(crate) enum DocBlobs {
    One { id: u32, blob: Vec<u8> },
    Many(HashMap<u32, Vec<u8>>),
}

impl DocBlobs {
    pub(crate) fn set(&mut self, id: u32, blob: Vec<u8>) {
        match self {
            DocBlobs::One { id: id0, blob: b0 } => {
                if *id0 == id {
                    *b0 = blob;
                } else {
                    let mut m = HashMap::with_capacity(2);
                    m.insert(*id0, std::mem::take(b0));
                    m.insert(id, blob);
                    *self = DocBlobs::Many(m);
                }
            }
            DocBlobs::Many(m) => {
                m.insert(id, blob);
            }
        }
    }

    pub(crate) fn get(&self, id: u32) -> Option<&[u8]> {
        match self {
            DocBlobs::One { id: id0, blob } => (*id0 == id).then_some(blob.as_slice()),
            DocBlobs::Many(m) => m.get(&id).map(Vec::as_slice),
        }
    }

    pub(crate) fn ids(&self) -> Vec<u32> {
        match self {
            DocBlobs::One { id, .. } => vec![*id],
            DocBlobs::Many(m) => m.keys().copied().collect(),
        }
    }

    /// Every (document, blob) pair — the field channel scores a token by
    /// walking its documents, where the position channel probes by id.
    pub(crate) fn each(&self) -> Vec<(u32, &[u8])> {
        match self {
            DocBlobs::One { id, blob } => vec![(*id, blob.as_slice())],
            DocBlobs::Many(m) => m.iter().map(|(id, b)| (*id, b.as_slice())).collect(),
        }
    }

    /// Remove `id`; `true` when no document is left, so the caller drops
    /// the token. Like `Buckets`, a shrinking `Many` is not demoted.
    /// (Production removals go through [`Channel::remove`], which
    /// maintains the byte counter; this stays for the unit tests.)
    #[cfg(test)]
    pub(crate) fn remove(&mut self, id: u32) -> bool {
        match self {
            DocBlobs::One { id: id0, .. } => *id0 == id,
            DocBlobs::Many(m) => {
                m.remove(&id);
                m.is_empty()
            }
        }
    }

    /// Heap bytes for this token's blobs: `One` pays only its blob's
    /// allocation, `Many` the power-of-two RawTable plus each blob's.
    /// (The walking half of the reference formula — see
    /// [`channel_bytes`].)
    #[cfg(test)]
    pub(crate) fn approx_bytes(&self) -> u64 {
        match self {
            DocBlobs::One { blob, .. } => blob_alloc(blob),
            DocBlobs::Many(m) => {
                let n = m.len() as u64;
                // RawTable capacity: next power of two above n / 0.875,
                // each bucket (u32, Vec<u8>) ≈ 32 B plus a control byte.
                let cap = (n * 8 / 7 + 1).next_power_of_two().max(4);
                cap * 33 + m.values().map(|b| blob_alloc(b)).sum::<u64>()
            }
        }
    }
}

/// One blob's real allocation: 16-byte granularity + header.
pub(crate) fn blob_alloc(b: &[u8]) -> u64 {
    (b.len().max(1) as u64).next_multiple_of(16) + 16
}

/// Approximate heap bytes for a whole token → blobs channel — the
/// walking reference formula. Production reads go through
/// [`Channel::bytes`], which maintains the same sum incrementally;
/// this walker remains as the invariant the tests hold the counter to.
///
/// Each outer entry pays its token-key Vec plus the [`DocBlobs`] enum
/// stored inline (its Vec / HashMap struct); the token's blob heap —
/// and, for a `Many` token, its RawTable — is added per variant.
#[cfg(test)]
pub(crate) fn channel_bytes(map: &HashMap<Vec<u8>, DocBlobs>) -> u64 {
    map.iter().map(|(t, db)| t.len() as u64 + 24 + 56 + db.approx_bytes()).sum()
}

/// The `Many` variant's RawTable term for `n` entries (the cap model
/// [`DocBlobs::approx_bytes`] uses, factored so the incremental
/// deltas and the walker cannot disagree).
fn many_table_bytes(n: u64) -> u64 {
    let cap = (n * 8 / 7 + 1).next_power_of_two().max(4);
    cap * 33
}

/// A token → [`DocBlobs`] channel whose heap-byte term is maintained
/// **incrementally**: every mutation applies an O(1) delta,
/// so reading the channel's share of the memory formula never walks
/// the map. The per-tick walk over these maps was the measured
/// write-load term behind a consumer's tiering idle-CPU report.
#[derive(Debug, Default)]
pub(crate) struct Channel {
    map: HashMap<Vec<u8>, DocBlobs>,
    bytes: u64,
}

impl Channel {
    pub(crate) fn get(&self, token: &[u8]) -> Option<&DocBlobs> {
        self.map.get(token)
    }

    /// Running Σ of [`channel_bytes`]'s per-token terms — O(1).
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Store `id`'s blob for `token`, replacing any prior blob.
    pub(crate) fn set(&mut self, token: &[u8], id: u32, blob: Vec<u8>) {
        match self.map.get_mut(token) {
            None => {
                self.bytes += token.len() as u64 + 24 + 56 + blob_alloc(&blob);
                self.map.insert(token.to_vec(), DocBlobs::One { id, blob });
            }
            Some(db) => {
                match db {
                    DocBlobs::One { id: id0, blob: b0 } if *id0 == id => {
                        self.bytes += blob_alloc(&blob);
                        self.bytes -= blob_alloc(b0);
                    }
                    // One → Many: the RawTable term appears.
                    DocBlobs::One { .. } => {
                        self.bytes += many_table_bytes(2) + blob_alloc(&blob);
                    }
                    DocBlobs::Many(m) => {
                        match m.get(&id) {
                            Some(old) => self.bytes -= blob_alloc(old),
                            None => {
                                let n = m.len() as u64;
                                self.bytes += many_table_bytes(n + 1) - many_table_bytes(n);
                            }
                        }
                        self.bytes += blob_alloc(&blob);
                    }
                }
                db.set(id, blob);
            }
        }
    }

    /// Drop `id` from `token`, removing the token entirely once its
    /// last document is gone.
    pub(crate) fn remove(&mut self, token: &[u8], id: u32) {
        let Some(db) = self.map.get_mut(token) else { return };
        match db {
            DocBlobs::One { id: id0, blob } => {
                if *id0 == id {
                    self.bytes -= token.len() as u64 + 24 + 56 + blob_alloc(blob);
                    self.map.remove(token);
                }
            }
            DocBlobs::Many(m) => {
                let Some(old) = m.remove(&id) else { return };
                self.bytes -= blob_alloc(&old);
                let n = m.len() as u64;
                if n == 0 {
                    self.bytes -= token.len() as u64 + 24 + 56 + many_table_bytes(1);
                    self.map.remove(token);
                } else {
                    self.bytes -= many_table_bytes(n + 1) - many_table_bytes(n);
                }
            }
        }
    }

    /// The walking reference sum — what [`Channel::bytes`] must always
    /// equal (the invariant the segment tests hold).
    #[cfg(test)]
    pub(crate) fn recompute(&self) -> u64 {
        channel_bytes(&self.map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip() {
        for case in
            [vec![], vec![0u32], vec![1, 2, 3], vec![5, 127, 128, 129, 16_383, 16_384, 2_000_000]]
        {
            let mut blob = Vec::new();
            for &v in &case {
                put_varint(&mut blob, v);
            }
            assert_eq!(get_varints(&blob), case, "roundtrip {case:?}");
        }
    }

    /// Six bytes that used to do two different wrong things.
    ///
    /// `80 80 80 80 80 01` is an overlong varint: five continuation bytes,
    /// so the sixth is shifted by 35. In a debug build that panicked —
    /// "attempt to shift left with overflow" — and in a release build the
    /// shift silently masked to 3 and the decoder returned `[8]`, a value
    /// nothing wrote. The blob is bytes; the two profiles disagreed about
    /// what they meant.
    #[test]
    fn an_overlong_varint_ends_the_stream_rather_than_the_process() {
        assert_eq!(get_varints(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]), Vec::<u32>::new());
        let mut at = 0;
        assert_eq!(next_varint(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01], &mut at), None);
    }

    /// The largest value that is not overlong still decodes. Without this
    /// the bound above could be off by one in the safe direction and no
    /// test would say so — the guard would be rejecting legal input.
    #[test]
    fn a_five_byte_varint_is_legal_and_u32_max_survives_a_roundtrip() {
        let mut blob = Vec::new();
        put_varint(&mut blob, u32::MAX);
        assert_eq!(blob.len(), 5, "u32::MAX needs every one of the five bytes");
        assert_eq!(get_varints(&blob), vec![u32::MAX]);
    }

    /// A blob that ends mid-varint yields what came before it and stops.
    #[test]
    fn a_trailing_continuation_byte_yields_no_value() {
        let mut blob = Vec::new();
        put_varint(&mut blob, 9);
        blob.push(0x80);
        assert_eq!(get_varints(&blob), vec![9]);
    }

    #[test]
    fn one_promotes_to_many_and_each_sees_both() {
        let mut db = DocBlobs::One { id: 7, blob: vec![1] };
        assert_eq!(db.get(7), Some(&[1u8][..]));
        db.set(7, vec![2]);
        assert_eq!(db.get(7), Some(&[2u8][..]), "same id overwrites in place");
        db.set(9, vec![3]);
        let mut got = db.each();
        got.sort_by_key(|(id, _)| *id);
        assert_eq!(got, vec![(7, &[2u8][..]), (9, &[3u8][..])]);
        assert!(!db.remove(7), "one of two left");
        assert!(db.remove(9), "last document gone");
    }
}
