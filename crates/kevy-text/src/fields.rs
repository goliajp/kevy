//! Per-field term frequencies — the side-channel that makes `IN <field…>`
//! a *field-scoped BM25* rather than a post-filter over merged scores.
//!
//! The ranking postings in [`crate::buckets`] store one term frequency
//! per (token, document): the sum of what each weighted field
//! contributed. That sum is the right thing to rank a whole document by,
//! but it cannot answer "how well does the **title** match", because the
//! title's contribution is no longer separable and the length
//! normalisation divides by the whole document.
//!
//! So this channel stores the *breakdown* the merged frequency is the sum
//! of: for every (token, document), the already-weighted per-field term
//! frequencies, plus the per-field token counts and corpus totals a
//! field-scoped length normalisation needs. Storing the weighted (not
//! raw) frequencies is what keeps the two paths from drifting — summing
//! this channel over every field reproduces the merged posting exactly,
//! weights as they were at index time.
//!
//! It is present only on a multi-field segment: with one field the
//! per-field numbers *are* the merged ones, so a single-field index pays
//! nothing. Like [`crate::positions`], it hangs off the hot path — an
//! unscoped query never touches it.

use crate::docblobs::{DocBlobs, channel_bytes, get_varints, put_varint};
use std::collections::HashMap;

/// The per-field side-channel of a multi-field segment.
#[derive(Debug)]
pub(crate) struct FieldStats {
    /// How many fields the index declares — the stride of every blob and
    /// of [`FieldStats::doc_len`].
    n: usize,
    /// token → (doc id → varint blob of weighted per-field tf, in field
    /// order).
    tf: HashMap<Vec<u8>, DocBlobs>,
    /// Per-field corpus token totals, for a field-scoped average length.
    total_len: Vec<u64>,
    /// id → per-field token counts, flat with stride `n` so a document
    /// costs no allocation of its own.
    doc_len: Vec<u32>,
}

impl FieldStats {
    pub(crate) fn new(n: usize) -> Self {
        Self { n, tf: HashMap::new(), total_len: vec![0; n], doc_len: Vec::new() }
    }

    /// Record `id`'s weighted per-field frequencies for `token`.
    pub(crate) fn set(&mut self, token: &[u8], id: u32, per_field: &[u32]) {
        let mut blob = Vec::with_capacity(per_field.len());
        for &v in per_field {
            put_varint(&mut blob, v);
        }
        match self.tf.get_mut(token) {
            Some(db) => db.set(id, blob),
            None => {
                self.tf.insert(token.to_vec(), DocBlobs::One { id, blob });
            }
        }
    }

    /// Drop `id` from `token`, removing the token once its last document
    /// is gone.
    pub(crate) fn remove(&mut self, token: &[u8], id: u32) {
        if let Some(db) = self.tf.get_mut(token)
            && db.remove(id)
        {
            self.tf.remove(token);
        }
    }

    /// Record `id`'s per-field token counts, growing the flat table as
    /// ids are handed out and correcting the corpus totals when a slot is
    /// reused.
    pub(crate) fn set_doc_len(&mut self, id: u32, per_field: &[u32]) {
        let base = id as usize * self.n;
        if self.doc_len.len() < base + self.n {
            self.doc_len.resize(base + self.n, 0);
        }
        for f in 0..self.n {
            let old = self.doc_len[base + f];
            let new = per_field.get(f).copied().unwrap_or(0);
            self.total_len[f] = self.total_len[f] - u64::from(old) + u64::from(new);
            self.doc_len[base + f] = new;
        }
    }

    /// Forget `id`'s lengths — the withdrawal counterpart of
    /// [`FieldStats::set_doc_len`].
    pub(crate) fn clear_doc_len(&mut self, id: u32) {
        self.set_doc_len(id, &[]);
    }

    /// Every document holding `token` in at least one of the `want`
    /// fields, with the summed weighted frequency **over those fields**.
    ///
    /// The filter and the sum happen in the same walk, so the caller gets
    /// an exact field-scoped document frequency (`.len()`) for free —
    /// summing stored per-field counts would double-count a document that
    /// holds the token in two of the wanted fields.
    pub(crate) fn docs_in(&self, token: &[u8], want: &[usize]) -> Vec<(u32, u32)> {
        let Some(db) = self.tf.get(token) else { return Vec::new() };
        let mut out = Vec::new();
        for (id, blob) in db.each() {
            let per_field = get_varints(blob);
            let tf: u32 = want
                .iter()
                .map(|&f| per_field.get(f).copied().unwrap_or(0))
                .sum();
            if tf > 0 {
                out.push((id, tf));
            }
        }
        out
    }

    /// `id`'s length over the `want` fields — the denominator a
    /// field-scoped BM25 normalises by.
    pub(crate) fn doc_len_in(&self, id: u32, want: &[usize]) -> u32 {
        let base = id as usize * self.n;
        want.iter()
            .map(|&f| self.doc_len.get(base + f).copied().unwrap_or(0))
            .sum()
    }

    /// Corpus token total over the `want` fields, for their average
    /// document length.
    pub(crate) fn total_len_in(&self, want: &[usize]) -> u64 {
        want.iter()
            .map(|&f| self.total_len.get(f).copied().unwrap_or(0))
            .sum()
    }

    /// `id`'s per-field lengths, for turning a concatenated token offset
    /// into "which field is this in" (field-scoped phrase queries).
    pub(crate) fn doc_lens(&self, id: u32) -> Vec<u32> {
        let base = id as usize * self.n;
        (0..self.n)
            .map(|f| self.doc_len.get(base + f).copied().unwrap_or(0))
            .collect()
    }

    /// How many fields the segment indexes.
    pub(crate) fn arity(&self) -> usize {
        self.n
    }

    /// Approximate heap bytes — the per-field term of the memory formula.
    pub(crate) fn approx_bytes(&self) -> u64 {
        channel_bytes(&self.tf)
            + (self.doc_len.capacity() as u64) * 4
            + (self.total_len.capacity() as u64) * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_walk_sums_only_wanted_fields() {
        let mut fs = FieldStats::new(3);
        // doc 1: token in fields 0 and 2; doc 2: field 1 only.
        fs.set(b"rust", 1, &[3, 0, 1]);
        fs.set(b"rust", 2, &[0, 5, 0]);

        let mut all = fs.docs_in(b"rust", &[0, 1, 2]);
        all.sort_unstable();
        assert_eq!(all, vec![(1, 4), (2, 5)], "every field = the merged tf");

        assert_eq!(fs.docs_in(b"rust", &[0]), vec![(1, 3)], "doc 2 has none in f0");
        assert_eq!(fs.docs_in(b"rust", &[1]), vec![(2, 5)]);
        let mut both = fs.docs_in(b"rust", &[0, 2]);
        both.sort_unstable();
        assert_eq!(both, vec![(1, 4)], "one document, counted once");
        assert!(fs.docs_in(b"absent", &[0]).is_empty());
    }

    #[test]
    fn lengths_track_reuse_of_an_id_slot() {
        let mut fs = FieldStats::new(2);
        fs.set_doc_len(0, &[10, 4]);
        fs.set_doc_len(1, &[2, 2]);
        assert_eq!(fs.total_len_in(&[0, 1]), 18);
        assert_eq!(fs.doc_len_in(0, &[0]), 10);
        assert_eq!(fs.doc_len_in(0, &[1]), 4);
        assert_eq!(fs.doc_lens(1), vec![2, 2]);

        // The id is freed and handed to a shorter document.
        fs.clear_doc_len(0);
        assert_eq!(fs.total_len_in(&[0, 1]), 4, "totals drop with the document");
        fs.set_doc_len(0, &[1, 1]);
        assert_eq!(fs.total_len_in(&[0, 1]), 6, "no double counting on reuse");
    }

    #[test]
    fn removing_the_last_document_drops_the_token() {
        let mut fs = FieldStats::new(2);
        fs.set(b"a", 1, &[1, 0]);
        fs.set(b"a", 2, &[0, 1]);
        fs.remove(b"a", 1);
        assert_eq!(fs.docs_in(b"a", &[0, 1]), vec![(2, 1)]);
        fs.remove(b"a", 2);
        assert!(fs.docs_in(b"a", &[0, 1]).is_empty());
        assert!(!fs.tf.contains_key(b"a".as_slice()), "empty token dropped");
    }
}
