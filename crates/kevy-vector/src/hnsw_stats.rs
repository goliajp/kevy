//! Read-only counters for [`Hnsw`] (child module via `#[path]`, the
//! `segment_stats.rs` house pattern), split from `hnsw.rs` for the
//! 500-LOC ceiling. The running-counter `stats()` and its walking
//! reference live side by side so they cannot drift apart unnoticed.

use super::{Hnsw, VectorStats};

impl Hnsw {
    /// Live (non-tombstoned) vectors — already tracked, so `O(1)`.
    /// [`Self::stats`] walks every node and every link to estimate bytes;
    /// a caller that only wants the count should not trigger that walk.
    pub fn vectors(&self) -> u64 {
        self.live
    }

    /// Counters — O(1) since v4.1-V5: `links_total` and `tombstones`
    /// are maintained at the three mutation sites (link push, shrink,
    /// tombstoning) instead of walking every node per call (this ran
    /// on every tiering tick). [`Self::recompute_stats`] is the
    /// walking reference the tests hold them to.
    pub fn stats(&self) -> VectorStats {
        self.stats_from(self.links_total, self.tombstones)
    }

    fn stats_from(&self, links: u64, tombstones: u64) -> VectorStats {
        let bytes_vec = (self.dim * 4) as u64;
        let approx_bytes: u64 = self.nodes.len() as u64 * (bytes_vec + 40)
            + links * 8
            + self.live * 32;
        VectorStats {
            vectors: self.live,
            tombstones,
            links,
            approx_bytes,
            rebuild_recommended: !self.nodes.is_empty() && tombstones * 10 > self.nodes.len() as u64 * 3,
        }
    }

    /// The walking reference — recomputes both counters from the live
    /// graph. Test-only: production reads the running counters.
    #[cfg(test)]
    pub fn recompute_stats(&self) -> VectorStats {
        let links: u64 = self.nodes.iter().map(|n| n.links.iter().map(Vec::len).sum::<usize>() as u64).sum();
        let tombstones = self.nodes.iter().filter(|n| n.dead).count() as u64;
        self.stats_from(links, tombstones)
    }

}
