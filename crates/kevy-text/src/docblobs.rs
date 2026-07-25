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

/// Decode a varint stream back to the values that were pushed.
pub(crate) fn get_varints(blob: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = 0u32;
    let mut shift = 0u32;
    for &b in blob {
        cur |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            out.push(cur);
            cur = 0;
            shift = 0;
        } else {
            shift += 7;
        }
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

/// Approximate heap bytes for a whole token → blobs channel.
///
/// Each outer entry pays its token-key Vec plus the [`DocBlobs`] enum
/// stored inline (its Vec / HashMap struct); the token's blob heap —
/// and, for a `Many` token, its RawTable — is added per variant.
pub(crate) fn channel_bytes(map: &HashMap<Vec<u8>, DocBlobs>) -> u64 {
    map.iter()
        .map(|(t, db)| t.len() as u64 + 24 + 56 + db.approx_bytes())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip() {
        for case in [
            vec![],
            vec![0u32],
            vec![1, 2, 3],
            vec![5, 127, 128, 129, 16_383, 16_384, 2_000_000],
        ] {
            let mut blob = Vec::new();
            for &v in &case {
                put_varint(&mut blob, v);
            }
            assert_eq!(get_varints(&blob), case, "roundtrip {case:?}");
        }
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
