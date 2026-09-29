//! The accounting contract, as a type.
//!
//! The accounting contract fixed these fields before this
//! crate existed, because both v5 RFCs state their ceiling as a
//! decomposition and a gate that cannot assert *"these terms sum to the
//! observed gap"* cannot check the only claim that matters.
//!
//! # The identity
//!
//! ```text
//! mapped == live + rounding + cache + span_free + virgin
//!         + hysteresis + segment_overhead
//! ```
//!
//! Exact, with no tolerance. Every mapped byte is in exactly one of
//! those states by construction, so a mismatch means something is
//! miscounted — and unexplained bytes are precisely where glibc's 2.24×
//! was hiding.
//!
//! # Two terms the contract did not have at T0
//!
//! T0 fixed five terms. Building the geometry showed the partition was
//! not a partition, so two were added — declared here with the reason,
//! which is what the contract requires (silently widening it is what is
//! banned, not changing it):
//!
//! - **`virgin`** — spans hand out slots by bumping a cursor, so the
//!   region above the cursor is *mapped but never touched*, and
//!   therefore not resident. Folding it into slack would have made the
//!   slack term look like memory when it is only address space. This
//!   split is the difference between a number that predicts RSS and one
//!   that does not.
//! - **`segment_overhead`** — one span per segment holds the header.
//!   1.6 % of every segment, structural and knowable, so it is named
//!   rather than absorbed into a neighbour.
//!
//! # Examples
//!
//! ```
//! use kevy_alloc::Heap;
//! let mut heap = Heap::new(0);
//! let p = heap.alloc(400, 8).ok_or("no mapping")?;
//! let s = heap.snapshot();
//! // the identity: every mapped byte sits in exactly one term
//! assert_eq!(s.mapped, s.accounted());
//! // SAFETY: `p` came from this heap with this size and alignment.
//! unsafe { heap.dealloc(p, 400, 8) };
//! # Ok::<(), &str>(())
//! ```

/// A snapshot of where every mapped byte is.
///
/// # Examples
///
/// ```
/// use kevy_alloc::{Heap, Stats};
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(100, 8).ok_or("no mapping")?;
/// let s: Stats = heap.snapshot();
/// assert_eq!(s.live, 100);
/// assert!(s.balanced());
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 100, 8) };
/// # Ok::<(), &str>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Stats {
    /// Total bytes mapped from the OS. The anchor.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// use kevy_alloc::segment::SEGMENT_BYTES;
    /// let mut heap = Heap::new(0);
    /// assert_eq!(heap.snapshot().mapped, 0);
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// // the first allocation maps one whole segment
    /// assert_eq!(heap.snapshot().mapped, SEGMENT_BYTES as u64);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 100, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub mapped: u64,
    /// Sum of `Layout::size()` over live allocations — what callers
    /// actually asked for, unrounded.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// assert_eq!(heap.snapshot().live, 100);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 100, 8) };
    /// assert_eq!(heap.snapshot().live, 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub live: u64,
    /// Sum of (slot size − requested size) over live allocations.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// // 100 bytes are served from the 104-byte class
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// assert_eq!(heap.snapshot().rounding, 4);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 100, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub rounding: u64,
    /// Bytes parked on foreign-free lists, waiting to be drained home.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut owner = Heap::new(1);
    /// let mut other = Heap::new(2);
    /// let p = owner.alloc(100, 8).ok_or("no mapping")?;
    /// // SAFETY: `p` came from `owner` with this size and alignment; freeing
    /// // it through another heap is the cross-shard path.
    /// unsafe { other.dealloc(p, 100, 8) };
    /// other.reclaim(); // ships the freed slot home
    /// assert_eq!(owner.snapshot().cache, 104);
    /// owner.drain_foreign();
    /// assert_eq!(owner.snapshot().cache, 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub cache: u64,
    /// Free slots in spans that were handed out before and returned to
    /// the free pool: touched, therefore resident.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let a = heap.alloc(100, 8).ok_or("no mapping")?;
    /// let before = heap.snapshot().span_free;
    /// // SAFETY: `a` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(a, 100, 8) };
    /// // the freed slot stays mapped and touched, ready for the next request
    /// assert_eq!(heap.snapshot().span_free, before + 104);
    /// # Ok::<(), &str>(())
    /// ```
    pub span_free: u64,
    /// Bytes whose pages have been handed back to the OS — mapped, not
    /// resident. The v2 term: this is what page-granular reclaim
    /// produces, and it did not exist while the reclaim unit was the
    /// whole span.
    ///
    /// Two shapes, both genuinely returned: free slots inside a span
    /// that is still live, and whole spans emptied and retired. The
    /// second used to be filed under `hysteresis`, so this read 0 while
    /// most of the map had in fact gone back.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let held: Vec<_> = (0..16_384).map(|_| heap.alloc(64, 8)).collect::<Option<_>>().ok_or("no mapping")?;
    /// for p in held {
    ///     // SAFETY: each came from this heap with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// // once they have gone unused for the purge delay
    /// for _ in 0..=kevy_alloc::PURGE_DELAY {
    ///     heap.reclaim();
    /// }
    /// let s = heap.snapshot();
    /// // pages go back only where the system page matches the span geometry
    /// assert_eq!(s.returned > 0, kevy_alloc::os::page_size_matches());
    /// # Ok::<(), &str>(())
    /// ```
    pub returned: u64,
    /// Mapped and never touched, therefore not resident: span bytes at
    /// or above the bump cursor, and whole spans carved with their
    /// segment that no class has ever claimed.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// let s = heap.snapshot();
    /// // most of a fresh segment has never been touched
    /// assert!(s.virgin > s.mapped / 2);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 100, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub virgin: u64,
    /// Retained rather than released, and therefore still resident:
    /// empty spans held for their class through the purge delay, spans
    /// whose discard the platform refused, and large mappings parked in
    /// the process-wide retention pool.
    ///
    /// Every byte here is one the allocator chose to keep. A byte that
    /// went back to the OS belongs in [`Self::returned`] — the two are
    /// opposites, and for the whole v5 arc they were the same number.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let held: Vec<_> = (0..16_384).map(|_| heap.alloc(64, 8)).collect::<Option<_>>().ok_or("no mapping")?;
    /// for p in held {
    ///     // SAFETY: each came from this heap with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// heap.reclaim();
    /// // inside the purge delay the emptied spans stay with their class
    /// assert!(heap.snapshot().hysteresis > 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub hysteresis: u64,
    /// Segment headers.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// use kevy_alloc::class::SPAN_BYTES;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// // one span of the segment holds its header
    /// assert_eq!(heap.snapshot().segment_overhead, SPAN_BYTES as u64);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 100, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub segment_overhead: u64,
    /// Live allocations served by direct mapping rather than a class.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(1 << 20, 8).ok_or("no mapping")?;
    /// // large blocks are counted process-wide, not per heap
    /// assert_eq!(heap.snapshot().large_count, 0);
    /// assert!(kevy_alloc::large_stats().large_count >= 1);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 1 << 20, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub large_count: u64,
    /// Spans currently assigned to a size class. Exported because "did
    /// we keep claiming fresh spans past reusable ones" is not visible
    /// in any byte count — the identity balances either way.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let a = heap.alloc(100, 8).ok_or("no mapping")?;
    /// let b = heap.alloc(4000, 8).ok_or("no mapping")?;
    /// // two classes in use, one span each
    /// assert_eq!(heap.snapshot().spans_assigned, 2);
    /// // SAFETY: both came from this heap with these sizes and alignment.
    /// unsafe {
    ///     heap.dealloc(a, 100, 8);
    ///     heap.dealloc(b, 4000, 8);
    /// }
    /// # Ok::<(), &str>(())
    /// ```
    pub spans_assigned: u64,
}

impl Stats {
    /// The sum the identity asserts. Kept separate from [`Self::mapped`]
    /// so a test can compare the two rather than trusting one.
    /// # Examples
    ///
    /// ```
    /// use kevy_alloc::Stats;
    /// // The identity this exists to check: every mapped byte is in
    /// // exactly one bucket, so the sum is comparable to `mapped`
    /// // rather than derived from it.
    /// let s = Stats::default();
    /// assert_eq!(s.accounted(), 0);
    /// assert_eq!(s.mapped, 0);
    /// ```
    #[must_use]
    pub fn accounted(&self) -> u64 {
        self.live
            + self.rounding
            + self.cache
            + self.span_free
            + self.returned
            + self.virgin
            + self.hysteresis
            + self.segment_overhead
    }

    /// Whether the identity holds exactly.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(3000, 8).ok_or("no mapping")?;
    /// assert!(heap.snapshot().balanced());
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 3000, 8) };
    /// assert!(heap.snapshot().balanced());
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn balanced(&self) -> bool {
        self.mapped == self.accounted()
    }

    /// Bytes we expect to be resident: everything mapped except what was
    /// never touched or has been handed back to the OS.
    ///
    /// An estimate by construction — the kernel decides residency, not
    /// us — so it is named as a prediction and compared against real RSS
    /// by the gate rather than substituted for it.
    ///
    /// `hysteresis` is NOT subtracted, and used to be. That term is what
    /// the policy is deliberately holding — empty spans kept for their
    /// class, large mappings parked in the retention pool — and holding
    /// is the opposite of handing back. Subtracting it made this
    /// prediction fall by exactly the amount the retention pool grew,
    /// which is the one direction it cannot be right in.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// let s = heap.snapshot();
    /// // untouched bytes are mapped but not expected to be resident
    /// assert_eq!(s.predicted_resident(), s.mapped - s.virgin - s.returned);
    /// assert!(s.predicted_resident() < s.mapped);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 100, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn predicted_resident(&self) -> u64 {
        self.mapped - self.virgin - self.returned
    }

    /// Add another heap's snapshot. Shards report separately; a process
    /// figure is their sum.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::{Heap, Stats};
    /// let (mut a, mut b) = (Heap::new(1), Heap::new(2));
    /// let pa = a.alloc(100, 8).ok_or("no mapping")?;
    /// let pb = b.alloc(200, 8).ok_or("no mapping")?;
    /// let mut total = Stats::default();
    /// total.merge(&a.snapshot());
    /// total.merge(&b.snapshot());
    /// assert_eq!(total.live, 300);
    /// assert!(total.balanced());
    /// // SAFETY: each came from its own heap with this size and alignment.
    /// unsafe {
    ///     a.dealloc(pa, 100, 8);
    ///     b.dealloc(pb, 200, 8);
    /// }
    /// # Ok::<(), &str>(())
    /// ```
    pub fn merge(&mut self, other: &Stats) {
        self.mapped += other.mapped;
        self.live += other.live;
        self.rounding += other.rounding;
        self.cache += other.cache;
        self.span_free += other.span_free;
        self.returned += other.returned;
        self.virgin += other.virgin;
        self.hysteresis += other.hysteresis;
        self.segment_overhead += other.segment_overhead;
        self.large_count += other.large_count;
        self.spans_assigned += other.spans_assigned;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_heap_balances() {
        assert!(Stats::default().balanced());
    }

    #[test]
    fn merge_is_additive_in_every_term() {
        let a = Stats { mapped: 10, live: 4, virgin: 6, ..Stats::default() };
        let mut sum = a;
        sum.merge(&a);
        assert_eq!(sum.mapped, 20);
        assert_eq!(sum.live, 8);
        assert_eq!(sum.virgin, 12);
        assert!(sum.balanced());
    }

    #[test]
    fn an_imbalance_is_visible() {
        let bad = Stats { mapped: 100, live: 1, ..Stats::default() };
        assert!(!bad.balanced());
        assert_eq!(bad.accounted(), 1);
    }
}
