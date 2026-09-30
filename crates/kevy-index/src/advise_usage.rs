//! Per-path usage counters: the refusal log's dual, the reclaim face's
//! raw material.

/// One declared path's usage counters — the refusal log's dual: that
/// log says what is missing, this says what goes unused (the reclaim
/// face's raw material). Plain relaxed atomics — the served-query
/// path pays two uncontended stores, never a lock.
///
/// The counters are atomics updated in place, so they stay private: a
/// reader takes a snapshot through [`UsageCell::read`] and
/// [`UsageCell::min_margin`].
///
/// ```
/// use kevy_index::UsageCell;
/// let c = UsageCell::declared_at(100);
/// c.hit(160);
/// c.probe(-5);
/// assert_eq!((c.read(), c.min_margin()), ((1, 160, 100), Some(-5)));
/// ```
#[derive(Debug)]
pub struct UsageCell {
    /// Queries served through this path.
    hits: std::sync::atomic::AtomicU64,
    /// Unix seconds of the most recent hit (0 = never).
    last_hit_s: std::sync::atomic::AtomicI64,
    /// Unix seconds the path was first seen declared — "never hit"
    /// is only meaningful with an age next to it (a path declared
    /// five seconds ago is not reclaim material).
    declared_s: std::sync::atomic::AtomicI64,
    /// Windowed paths only: the smallest `lower_bound - boundary`
    /// any query has probed (`i64::MAX` = never observed). A margin
    /// that never goes non-positive means no query has touched the
    /// cold side — the window-narrowing advice's whole input, no max
    /// tracking needed (boundary ≈ max − span, within a bucket).
    min_margin: std::sync::atomic::AtomicI64,
}

/// A zeroed cell with the margin UNOBSERVED (`i64::MAX`) — a derived
/// all-zeroes default would read as "a query probed margin 0".
impl Default for UsageCell {
    fn default() -> Self {
        use std::sync::atomic::{AtomicI64, AtomicU64};
        Self {
            hits: AtomicU64::new(0),
            last_hit_s: AtomicI64::new(0),
            declared_s: AtomicI64::new(0),
            min_margin: AtomicI64::new(i64::MAX),
        }
    }
}

impl UsageCell {
    /// A fresh cell for a path first seen declared at `now_s`.
    #[must_use]
    pub fn declared_at(now_s: i64) -> Self {
        let c = Self::default();
        c.declared_s.store(now_s, std::sync::atomic::Ordering::Relaxed);
        c
    }

    /// Record one windowed query's probe depth (`lower - boundary`).
    pub fn probe(&self, margin: i64) {
        self.min_margin.fetch_min(margin, std::sync::atomic::Ordering::Relaxed);
    }

    /// Count one served query at `now_s` (unix seconds).
    pub fn hit(&self, now_s: i64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.hits.fetch_add(1, Relaxed);
        self.last_hit_s.store(now_s, Relaxed);
    }

    /// Windowed paths only: the smallest `lower - boundary` margin any
    /// query has probed, or `None` when no query has been observed.
    ///
    /// ```
    /// let c = kevy_index::UsageCell::default();
    /// assert_eq!(c.min_margin(), None);
    /// c.probe(40);
    /// c.probe(12);
    /// assert_eq!(c.min_margin(), Some(12));
    /// ```
    #[must_use]
    pub fn min_margin(&self) -> Option<i64> {
        let m = self.min_margin.load(std::sync::atomic::Ordering::Relaxed);
        (m != i64::MAX).then_some(m)
    }

    /// `(hits, last_hit_s, declared_s)` snapshot; `last_hit_s` is 0
    /// until the first hit.
    #[must_use]
    pub fn read(&self) -> (u64, i64, i64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.hits.load(Relaxed), self.last_hit_s.load(Relaxed), self.declared_s.load(Relaxed))
    }
}
