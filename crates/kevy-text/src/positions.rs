//! Positional postings — a physical side-channel to the impact-bucketed
//! BM25 postings in [`crate::buckets`], filled only when an index is
//! created `WITH POSITIONS`. Phrase, proximity and highlight all need to
//! know *where* in a document each term occurred; ranking does not, so
//! the positions live off the BM25 hot path and a segment without them
//! (`None`) is byte-identical to the pre-positions structure.
//!
//! Layout: token → (doc id → delta+varint blob of ascending token
//! offsets), over the shared [`crate::docblobs`] storage. Offsets are a
//! token's ordinals within a document's concatenated fields (field
//! order), so a phrase check decodes two blobs and walks them in
//! lockstep. Delta+varint keeps a high-frequency term from paying 4
//! bytes per occurrence — the standard Lucene positions layout.

use crate::docblobs::{Channel, DocBlobs, get_varints, put_varint};

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

/// Walk a delta+varint blob's ascending offsets without building a
/// `Vec` — the allocation-free twin of [`decode`], for the hot phrase
/// check.
pub(crate) fn walk(blob: &[u8]) -> impl Iterator<Item = u32> + '_ {
    let mut acc = 0u32;
    let mut cur = 0u32;
    let mut shift = 0u32;
    blob.iter().filter_map(move |&b| {
        cur |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            acc += cur;
            cur = 0;
            shift = 0;
            Some(acc)
        } else {
            shift += 7;
            None
        }
    })
}

/// Decode a delta+varint blob back to ascending offsets.
fn decode(blob: &[u8]) -> Vec<u32> {
    let mut acc = 0u32;
    get_varints(blob)
        .into_iter()
        .map(|d| {
            acc += d;
            acc
        })
        .collect()
}

/// The positional side-channel: token → per-document position blobs.
/// Present only on a `WITH POSITIONS` segment.
#[derive(Debug, Default)]
pub(crate) struct Positions {
    chan: Channel,
}

impl Positions {
    /// Store `id`'s ascending offsets for `token`.
    pub(crate) fn set(&mut self, token: &[u8], id: u32, offsets: &[u32]) {
        self.chan.set(token, id, encode(offsets));
    }

    /// Drop `id` from `token`'s postings, removing the token entirely
    /// once its last document is gone.
    pub(crate) fn remove(&mut self, token: &[u8], id: u32) {
        self.chan.remove(token, id);
    }

    /// `id`'s raw position blob for `token`, undecoded.
    ///
    /// The decoding form below allocates a `Vec` per call, which a phrase
    /// check makes once per candidate document per token — on a phrase of
    /// two head terms that is two allocations per document in the corpus.
    /// Handing back the bytes lets a caller walk them in place, worth a
    /// measured 6.2% of phrase p95.
    ///
    /// An earlier version of this comment blamed a profile showing 87% of
    /// query time in the allocator. That profile was of the shard's
    /// teardown, not its queries — see bench/profile-textgate.sh. The
    /// allocations described here are real and removing them helped; the
    /// 87% was measuring something else entirely.
    pub(crate) fn blob(&self, token: &[u8], id: u32) -> Option<&[u8]> {
        self.chan.get(token)?.get(id)
    }

    /// `id`'s decoded ascending offsets for `token`, or `None` when the
    /// document does not contain it.
    pub(crate) fn get(&self, token: &[u8], id: u32) -> Option<Vec<u32>> {
        self.chan.get(token)?.get(id).map(decode)
    }

    /// Documents containing `token`, as ids — the phrase-query candidate
    /// set for that token.
    pub(crate) fn ids(&self, token: &[u8]) -> Vec<u32> {
        self.chan.get(token).map(DocBlobs::ids).unwrap_or_default()
    }

    /// Approximate heap bytes — the positions term of the memory formula
    /// (the memory gate calibrates it against real RSS growth).
    /// O(1) — maintained incrementally by the channel (v4.1-V5).
    pub(crate) fn approx_bytes(&self) -> u64 {
        self.chan.bytes()
    }

    /// The walking reference for the invariant test.
    #[cfg(test)]
    pub(crate) fn recompute_bytes(&self) -> u64 {
        self.chan.recompute()
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
        p.set(b"a", 1, &[0, 5, 9]);
        p.set(b"a", 2, &[3]);
        p.set(b"b", 1, &[1]);
        assert_eq!(p.get(b"a", 1), Some(vec![0, 5, 9]));
        assert_eq!(p.get(b"a", 2), Some(vec![3]));
        assert_eq!(p.get(b"a", 3), None);
        let mut ids = p.ids(b"a");
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);

        p.remove(b"a", 1);
        assert_eq!(p.get(b"a", 1), None);
        assert_eq!(p.get(b"a", 2), Some(vec![3]));
        p.remove(b"b", 1);
        assert_eq!(p.ids(b"b"), Vec::<u32>::new(), "empty token dropped");
    }

    #[test]
    fn approx_bytes_grows_with_content() {
        let mut p = Positions::default();
        let empty = p.approx_bytes();
        p.set(b"token", 1, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert!(p.approx_bytes() > empty, "storing positions costs bytes");
    }
}
