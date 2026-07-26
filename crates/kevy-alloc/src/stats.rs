//! The accounting contract, as a type.
//!
//! `bench/V5-ACCOUNTING-CONTRACT.md` §1 fixes these fields before this
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

/// A snapshot of where every mapped byte is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Total bytes mapped from the OS. The anchor.
    pub mapped: u64,
    /// Sum of `Layout::size()` over live allocations — what callers
    /// actually asked for, unrounded.
    pub live: u64,
    /// Sum of (slot size − requested size) over live allocations.
    pub rounding: u64,
    /// Bytes parked on foreign-free lists, waiting to be drained home.
    pub cache: u64,
    /// Free slots in spans that were handed out before and returned:
    /// touched, therefore resident.
    pub span_free: u64,
    /// Span bytes at or above the bump cursor — mapped, never touched,
    /// not resident.
    pub virgin: u64,
    /// Whole spans with nothing live, retained rather than released.
    pub hysteresis: u64,
    /// Segment headers.
    pub segment_overhead: u64,
    /// Live allocations served by direct mapping rather than a class.
    pub large_count: u64,
    /// Spans currently assigned to a size class. Exported because "did
    /// we keep claiming fresh spans past reusable ones" is not visible
    /// in any byte count — the identity balances either way.
    pub spans_assigned: u64,
}

impl Stats {
    /// The sum the identity asserts. Kept separate from [`Self::mapped`]
    /// so a test can compare the two rather than trusting one.
    #[must_use]
    pub fn accounted(&self) -> u64 {
        self.live
            + self.rounding
            + self.cache
            + self.span_free
            + self.virgin
            + self.hysteresis
            + self.segment_overhead
    }

    /// Whether the identity holds exactly.
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
    #[must_use]
    pub fn predicted_resident(&self) -> u64 {
        self.mapped - self.virgin - self.hysteresis
    }

    /// Add another heap's snapshot. Shards report separately; a process
    /// figure is their sum.
    pub fn merge(&mut self, other: &Stats) {
        self.mapped += other.mapped;
        self.live += other.live;
        self.rounding += other.rounding;
        self.cache += other.cache;
        self.span_free += other.span_free;
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
