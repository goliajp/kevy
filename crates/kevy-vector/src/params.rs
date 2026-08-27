//! The two public types an HNSW index is configured and measured by.
//!
//! Split out of `hnsw` in v6, which had reached the 500-line ceiling.
//! These are the crate's outward-facing knobs and counters; neither knows
//! anything about the graph, and separating them keeps `hnsw` to the
//! algorithm.

use crate::dist::Distance;

/// Construction/search parameters (immutable once built).
#[derive(Debug, Clone, Copy)]
pub struct HnswParams {
    /// Max bidirectional links per node per layer (layer 0 gets 2M).
    pub m: usize,
    /// Construction beam width.
    pub ef_construction: usize,
    /// Metric.
    pub distance: Distance,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self { m: 16, ef_construction: 200, distance: Distance::Cosine }
    }
}

/// Sizing counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorStats {
    /// Living vectors.
    pub vectors: u64,
    /// Tombstoned nodes still in the graph.
    pub tombstones: u64,
    /// Total graph links.
    pub links: u64,
    /// Approximate heap bytes.
    pub approx_bytes: u64,
    /// 1 when tombstones exceed the rebuild threshold (30%).
    pub rebuild_recommended: bool,
}
