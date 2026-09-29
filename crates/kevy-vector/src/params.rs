//! The two public types an HNSW index is configured and measured by.
//!
//! Split out of `hnsw` in v6, which had reached the 500-line ceiling.
//! These are the crate's outward-facing knobs and counters; neither knows
//! anything about the graph, and separating them keeps `hnsw` to the
//! algorithm.

use crate::dist::Distance;

/// Construction/search parameters (immutable once built).
///
/// Start from [`HnswParams::default`] and set what differs, with the
/// `with_*` builders or by assigning the fields.
///
/// # Examples
///
/// ```
/// use kevy_vector::{HnswParams, Distance};
/// let d = HnswParams::default();
/// // The declaration-time knobs, and what a caller gets without naming
/// // any of them.
/// assert_eq!((d.m, d.ef_construction), (16, 200));
/// assert_eq!(d.distance, Distance::Cosine);
///
/// let wide = HnswParams::default().with_ef_construction(400);
/// assert_eq!(wide.m, 16, "the rest carries over");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

impl HnswParams {
    /// Set [`HnswParams::m`], the links per node per layer.
    ///
    /// ```
    /// assert_eq!(kevy_vector::HnswParams::default().with_m(32).m, 32);
    /// ```
    #[must_use]
    pub fn with_m(mut self, m: usize) -> Self {
        self.m = m;
        self
    }

    /// Set [`HnswParams::ef_construction`], the construction beam width.
    ///
    /// ```
    /// let p = kevy_vector::HnswParams::default().with_ef_construction(64);
    /// assert_eq!(p.ef_construction, 64);
    /// ```
    #[must_use]
    pub fn with_ef_construction(mut self, ef_construction: usize) -> Self {
        self.ef_construction = ef_construction;
        self
    }

    /// Set [`HnswParams::distance`], the metric.
    ///
    /// ```
    /// use kevy_vector::{Distance, HnswParams};
    /// assert_eq!(HnswParams::default().with_distance(Distance::L2).distance, Distance::L2);
    /// ```
    #[must_use]
    pub fn with_distance(mut self, distance: Distance) -> Self {
        self.distance = distance;
        self
    }
}

/// Sizing counters.
///
/// # Examples
///
/// ```
/// use kevy_vector::{Hnsw, HnswParams};
/// let mut h = Hnsw::new(2, HnswParams::default());
/// h.apply(b"a", Some(vec![1.0, 0.0]));
/// h.apply(b"b", Some(vec![0.0, 1.0]));
/// let s = h.stats();
/// assert_eq!(s.vectors, 2);
/// assert_eq!(s.tombstones, 0);
/// assert!(!s.rebuild_recommended);
///
/// // A removal leaves a tombstone behind rather than rewriting the graph.
/// h.apply(b"a", None);
/// let s = h.stats();
/// assert_eq!(s.vectors, 1);
/// assert_eq!(s.tombstones, 1);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
