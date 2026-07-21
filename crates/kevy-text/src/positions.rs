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

/// The positional side-channel: token → (doc id → position blob).
/// Present only on a `WITH POSITIONS` segment.
#[derive(Debug, Default)]
pub(crate) struct Positions {
    map: HashMap<Vec<u8>, HashMap<u32, Vec<u8>>>,
}

impl Positions {
    /// Store `id`'s ascending offsets for `token`.
    pub(crate) fn set(&mut self, token: &[u8], id: u32, offsets: &[u32]) {
        self.map
            .entry(token.to_vec())
            .or_default()
            .insert(id, encode(offsets));
    }

    /// Drop `id` from `token`'s postings, removing the token entirely
    /// once its last document is gone.
    pub(crate) fn remove(&mut self, token: &[u8], id: u32) {
        if let Some(inner) = self.map.get_mut(token) {
            inner.remove(&id);
            if inner.is_empty() {
                self.map.remove(token);
            }
        }
    }

    /// `id`'s decoded ascending offsets for `token`, or `None` when the
    /// document does not contain it.
    pub(crate) fn get(&self, token: &[u8], id: u32) -> Option<Vec<u32>> {
        self.map.get(token)?.get(&id).map(|b| decode(b))
    }

    /// Documents containing `token`, as ids — the phrase-query candidate
    /// set for that token.
    pub(crate) fn ids(&self, token: &[u8]) -> Vec<u32> {
        self.map
            .get(token)
            .map(|inner| inner.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Approximate heap bytes — the positions term of the memory formula
    /// (the memory gate calibrates it against real RSS growth).
    ///
    /// A first cut charged a flat `blob.len() + 30` per posting and
    /// underestimated real growth ~3× at 1M docs: it ignored that every
    /// token owns a nested `HashMap<u32, Vec<u8>>` — a struct plus a
    /// power-of-two RawTable even for a single hapax posting — and that
    /// each tiny positions blob is a separate heap allocation the system
    /// allocator rounds up and headers. This models both.
    pub(crate) fn approx_bytes(&self) -> u64 {
        self.map
            .iter()
            .map(|(t, inner)| {
                let n = inner.len() as u64;
                // Inner RawTable: capacity is the next power of two above
                // n / 0.875 (its load factor); each bucket holds
                // (u32, Vec<u8>) ≈ 32 B plus a 1-byte control.
                let cap = (n * 8 / 7 + 1).next_power_of_two().max(4);
                let table = cap * 33;
                // Each blob is its own allocation: 16-byte granularity
                // plus a per-allocation header.
                let blobs: u64 = inner
                    .values()
                    .map(|b| (b.len().max(1) as u64).next_multiple_of(16) + 16)
                    .sum();
                // Outer entry: the token-key Vec and the inner map struct.
                t.len() as u64 + 24 + 48 + table + blobs
            })
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
