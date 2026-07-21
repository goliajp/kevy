//! Read-only counters and accessors for [`TextSegment`], split from
//! `segment.rs` for the 500-LOC house rule. A child module (declared in
//! `segment.rs`), so it reaches the segment's private fields.
//!
//! `stats` is the measured side of the documented memory formula
//! (`bench/textgate.sh` clamps this against real RSS growth). The three
//! accessors below are the pass-1 API a cross-shard query sums for
//! global BM25 (step 4b).

use super::{TextSegment, TextStats};
use crate::buckets::Buckets;
use crate::fields::FieldStats;
use crate::positions::Positions;

impl TextSegment {
    /// Live counters.
    pub fn stats(&self) -> TextStats {
        let postings: u64 = self.postings.values().map(|l| l.len() as u64).sum();
        TextStats {
            docs: self.docs.len() as u64,
            tokens: self.postings.len() as u64,
            postings,
            approx_bytes: self.approx_bytes(),
        }
    }

    /// The measured heap estimate: token keys + per-`Many`-posting
    /// structure + the docs/id tables. Hapax (`One`) lists are inline,
    /// so only `Many` lists pay the band-vec + index cost.
    fn approx_bytes(&self) -> u64 {
        let many_postings: u64 = self
            .postings
            .values()
            .map(|l| match l {
                Buckets::One { .. } => 0,
                Buckets::Many(m) => m.index.len() as u64,
            })
            .sum();
        let token_bytes: u64 = self.postings.keys().map(|t| (t.len() + 48) as u64).sum();
        // docs table + the id→key / id→dl tables (key stored twice);
        // docs keep each field's text so an update re-derives tokens.
        let doc_bytes: u64 = self
            .docs
            .iter()
            .map(|(k, (_, _, fields))| {
                let text: usize = fields.iter().map(|(t, _)| t.len() + 4).sum();
                (2 * k.len() + text + 110) as u64
            })
            .sum();
        // per-Many-posting ≈ 4B band-vec slot + ~26B list-index entry.
        // The side-channels add their own terms when present (`WITH
        // POSITIONS`, and the per-field breakdown of a multi-field
        // index); absent, each contributes nothing, so a plain
        // single-field segment's formula is byte-identical.
        let position_bytes = self.positions.as_ref().map_or(0, Positions::approx_bytes);
        let field_bytes = self.fields.as_ref().map_or(0, FieldStats::approx_bytes);
        token_bytes + many_postings * 30 + doc_bytes + position_bytes + field_bytes
    }

    /// Verify hook: is `key` indexed here?
    pub fn contains(&self, key: &[u8]) -> bool {
        self.docs.contains_key(key)
    }

    /// This shard's total document length in tokens (unweighted) — one
    /// of the three numbers a cross-shard query sums for global BM25
    /// (step 4b, pass 1): global avgdl = Σ total_len / Σ n_docs.
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// This shard's document frequency for `token` — how many local
    /// documents contain it. Summed across shards for one query token's
    /// global df. `0` when the token is absent here.
    pub fn local_df(&self, token: &[u8]) -> u32 {
        self.postings.get(token).map_or(0, Buckets::len) as u32
    }
}
