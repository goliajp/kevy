//! The read-only index admin surface (stats and enumeration). A
//! `#[path]` child of `ops_index.rs`, split under the 500-LOC rule.

use kevy_index::IndexKind;

use crate::KevyResult;
use crate::ops_index::SegmentStats;
use crate::store::Store;

impl Store {
    /// Summed segment stats (entries / bytes / coerce failures /
    /// unique-fence duplicates).
    pub fn idx_stats(&self, name: &[u8]) -> KevyResult<SegmentStats> {
        let mut sum = SegmentStats::default();
        self.for_each_segment(name, |seg| {
            let s = seg.stats();
            sum.entries += s.entries;
            sum.approx_bytes += s.approx_bytes;
            sum.coerce_failures += s.coerce_failures;
            sum.duplicates += s.duplicates;
        })?;
        Ok(sum)
    }

    /// Declared indexes (name, prefix, kind), declaration order.
    pub fn idx_list(&self) -> Vec<(Vec<u8>, Vec<u8>, IndexKind)> {
        let g = self.indexes.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.iter().map(|(s, _)| (s.name.clone(), s.prefix.clone(), s.kind)).collect()
    }
}
