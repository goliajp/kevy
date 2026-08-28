//! Read-only counters for [`Hnsw`] (child module via `#[path]`, the
//! `segment_stats.rs` house pattern), split from `hnsw.rs` for the
//! 500-LOC ceiling. The running-counter `stats()` and its walking
//! reference live side by side so they cannot drift apart unnoticed.

use super::{Hnsw, VectorStats};

impl Hnsw {
    /// Live (non-tombstoned) vectors — already tracked, so `O(1)`.
    /// [`Self::stats`] walks every node and every link to estimate bytes;
    /// a caller that only wants the count should not trigger that walk.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// h.apply(b"b", Some(vec![0.0, 1.0]));
    /// h.apply(b"a", None);
    /// // Live only: the tombstone left behind does not count.
    /// assert_eq!(h.vectors(), 1);
    /// assert_eq!(h.stats().tombstones, 1);
    /// ```
    pub fn vectors(&self) -> u64 {
        self.live
    }

    /// Counters — O(1): `links_total` and `tombstones`
    /// are maintained at the three mutation sites (link push, shrink,
    /// tombstoning) instead of walking every node per call (this ran
    /// on every tiering tick). [`Self::recompute_stats`] is the
    /// walking reference the tests hold them to.
    /// # Examples
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(4, HnswParams::default());
    /// for i in 0..3u8 {
    ///     h.apply(&[i], Some(vec![f32::from(i), 1.0, 0.0, 0.0]));
    /// }
    /// let s = h.stats();
    /// assert_eq!(s.vectors, 3);
    /// // The sizing formula IDX.LIST reports: payload, links and node
    /// // overhead, not a measured heap.
    /// assert!(s.approx_bytes >= 3 * (4 * 4 + 40));
    /// ```
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
