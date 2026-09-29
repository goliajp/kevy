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
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let links = |m| {
    ///     let mut h = Hnsw::new(2, HnswParams::default().with_m(m));
    ///     for i in 0..40u8 {
    ///         h.apply(&[i], Some(vec![f32::from(i) + 1.0, 1.0]));
    ///     }
    ///     h.stats().links
    /// };
    /// // A wider degree keeps more neighbours per node.
    /// assert!(links(2) < links(16));
    /// ```
    pub m: usize,
    /// Construction beam width.
    ///
    /// Wider builds a better-connected graph at a higher insert cost.
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let p = HnswParams::default().with_ef_construction(8);
    /// assert_eq!(p.ef_construction, 8);
    /// let mut h = Hnsw::new(2, p);
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// h.apply(b"b", Some(vec![0.0, 1.0]));
    /// assert_eq!(h.knn(&[1.0, 0.1], 1, 8)[0].0, b"a");
    /// ```
    pub ef_construction: usize,
    /// Metric.
    ///
    /// ```
    /// use kevy_vector::{Distance, HnswParams};
    /// assert_eq!(HnswParams::default().distance, Distance::Cosine);
    /// let p = HnswParams::default().with_distance(Distance::Ip);
    /// assert_eq!(p.distance.tag(), "ip");
    /// ```
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
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// h.apply(b"a", Some(vec![0.0, 1.0]));
    /// assert_eq!(h.stats().vectors, 1, "a rewrite of the same key is one vector");
    /// ```
    pub vectors: u64,
    /// Tombstoned nodes still in the graph.
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// h.apply(b"b", Some(vec![0.0, 1.0]));
    /// h.apply(b"a", None);
    /// assert_eq!(h.stats().tombstones, 1);
    /// h.rebuild();
    /// assert_eq!(h.stats().tombstones, 0, "a rebuild drops them");
    /// ```
    pub tombstones: u64,
    /// Total graph links.
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// h.apply(b"a", Some(vec![1.0, 0.0]));
    /// assert_eq!(h.stats().links, 0, "a lone node links to nothing");
    /// h.apply(b"b", Some(vec![0.0, 1.0]));
    /// assert!(h.stats().links >= 2, "the second node links both ways");
    /// ```
    pub links: u64,
    /// Approximate heap bytes.
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(8, HnswParams::default());
    /// let empty = h.stats().approx_bytes;
    /// h.apply(b"a", Some(vec![1.0; 8]));
    /// // At least the vector's own 8 * 4 bytes were added.
    /// assert!(h.stats().approx_bytes >= empty + 32);
    /// ```
    pub approx_bytes: u64,
    /// 1 when tombstones exceed the rebuild threshold (30%).
    ///
    /// ```
    /// use kevy_vector::{Hnsw, HnswParams};
    /// let mut h = Hnsw::new(2, HnswParams::default());
    /// for i in 0..10u8 {
    ///     h.apply(&[i], Some(vec![f32::from(i) + 1.0, 1.0]));
    /// }
    /// for i in 0..3u8 {
    ///     h.apply(&[i], None);
    /// }
    /// assert!(!h.stats().rebuild_recommended, "3 of 10 is not over 30%");
    /// h.apply(&[3], None);
    /// assert!(h.stats().rebuild_recommended);
    /// h.rebuild();
    /// assert!(!h.stats().rebuild_recommended);
    /// ```
    pub rebuild_recommended: bool,
}
