//! Positional postings — a physical side-channel to the impact-bucketed
//! BM25 postings in [`crate::buckets`], filled only when an index is
//! created `WITH POSITIONS`. Phrase, proximity and highlight all need to
//! know *where* in a document each term occurred; ranking does not, so
//! the positions live off the BM25 hot path and a segment without them
//! (`None`) is byte-identical to the pre-positions structure.
//!
//! Layout: token → (doc id → delta+varint blob of ascending token
//! offsets). Offsets are a token's ordinals within a document's
//! concatenated fields (field order), so a phrase check decodes two
//! blobs and walks them in lockstep. Delta+varint keeps a
//! high-frequency term from paying 4 bytes per occurrence — the standard
//! Lucene positions layout.

use std::collections::HashMap;

/// LEB128-encode one delta onto `out`.
fn put_varint(out: &mut Vec<u8>, mut v: u32) {
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

/// Encode ascending `offsets` as a delta+varint blob. Offsets are
/// strictly ascending (distinct ordinals), so every delta after the
/// first is ≥ 1 and the subtraction never underflows.
fn encode(offsets: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0u32;
    for &p in offsets {
        put_varint(&mut out, p - prev);
        prev = p;
    }
    out
}

/// Decode a delta+varint blob back to ascending offsets.
fn decode(blob: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut cur = 0u32;
    let mut shift = 0u32;
    for &b in blob {
        cur |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            acc += cur;
            out.push(acc);
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
enum DocBlobs {
    One { id: u32, blob: Vec<u8> },
    Many(HashMap<u32, Vec<u8>>),
}

impl DocBlobs {
    fn set(&mut self, id: u32, blob: Vec<u8>) {
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

    fn get(&self, id: u32) -> Option<&[u8]> {
        match self {
            DocBlobs::One { id: id0, blob } => (*id0 == id).then_some(blob.as_slice()),
            DocBlobs::Many(m) => m.get(&id).map(Vec::as_slice),
        }
    }

    fn ids(&self) -> Vec<u32> {
        match self {
            DocBlobs::One { id, .. } => vec![*id],
            DocBlobs::Many(m) => m.keys().copied().collect(),
        }
    }

    /// Remove `id`; `true` when no document is left, so the caller drops
    /// the token. Like `Buckets`, a shrinking `Many` is not demoted.
    fn remove(&mut self, id: u32) -> bool {
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
    fn approx_bytes(&self) -> u64 {
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

/// One position blob's real allocation: 16-byte granularity + header.
fn blob_alloc(b: &[u8]) -> u64 {
    (b.len().max(1) as u64).next_multiple_of(16) + 16
}

/// The positional side-channel: token → per-document position blobs.
/// Present only on a `WITH POSITIONS` segment.
#[derive(Debug, Default)]
pub(crate) struct Positions {
    map: HashMap<Vec<u8>, DocBlobs>,
}

impl Positions {
    /// Store `id`'s ascending offsets for `token`.
    pub(crate) fn set(&mut self, token: &[u8], id: u32, offsets: &[u32]) {
        let blob = encode(offsets);
        match self.map.get_mut(token) {
            Some(db) => db.set(id, blob),
            None => {
                self.map.insert(token.to_vec(), DocBlobs::One { id, blob });
            }
        }
    }

    /// Drop `id` from `token`'s postings, removing the token entirely
    /// once its last document is gone.
    pub(crate) fn remove(&mut self, token: &[u8], id: u32) {
        if let Some(db) = self.map.get_mut(token)
            && db.remove(id)
        {
            self.map.remove(token);
        }
    }

    /// `id`'s decoded ascending offsets for `token`, or `None` when the
    /// document does not contain it.
    pub(crate) fn get(&self, token: &[u8], id: u32) -> Option<Vec<u32>> {
        self.map.get(token)?.get(id).map(decode)
    }

    /// Documents containing `token`, as ids — the phrase-query candidate
    /// set for that token.
    pub(crate) fn ids(&self, token: &[u8]) -> Vec<u32> {
        self.map.get(token).map(DocBlobs::ids).unwrap_or_default()
    }

    /// Approximate heap bytes — the positions term of the memory formula
    /// (the memory gate calibrates it against real RSS growth).
    ///
    /// Each outer entry pays its token-key Vec plus the [`DocBlobs`] enum
    /// stored inline (its Vec / HashMap struct); the token's blob heap —
    /// and, for a `Many` token, its RawTable — is added per variant.
    pub(crate) fn approx_bytes(&self) -> u64 {
        self.map
            .iter()
            .map(|(t, db)| t.len() as u64 + 24 + 56 + db.approx_bytes())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_delta_roundtrips() {
        for case in [
            vec![],
            vec![0u32],
            vec![0, 1, 2, 3],
            vec![5, 130, 131, 16_500, 2_000_000],
            vec![0, 127, 128, 129, 16_383, 16_384],
        ] {
            assert_eq!(decode(&encode(&case)), case, "roundtrip {case:?}");
        }
    }

    #[test]
    fn set_get_remove() {
        let mut p = Positions::default();
        p.set(b"quick", 7, &[0, 4, 9]);
        p.set(b"brown", 7, &[1, 5]);
        p.set(b"quick", 3, &[2]);
        assert_eq!(p.get(b"quick", 7), Some(vec![0, 4, 9]));
        assert_eq!(p.get(b"brown", 7), Some(vec![1, 5]));
        assert_eq!(p.get(b"quick", 99), None);
        assert_eq!(p.get(b"missing", 7), None);
        let mut ids = p.ids(b"quick");
        ids.sort_unstable();
        assert_eq!(ids, vec![3, 7]);
        p.remove(b"quick", 7);
        assert_eq!(p.get(b"quick", 7), None);
        assert_eq!(p.ids(b"quick"), vec![3]);
        p.remove(b"quick", 3);
        assert!(p.ids(b"quick").is_empty(), "token dropped when last doc gone");
    }
}
