//! [`Segment::tidy`]: the background repack's entry point.

use crate::segment::Segment;

impl Segment {
    /// Pack neighbouring leaves, visiting at most `leaves` of them; returns
    /// whether more work remains. Writes never pack: a leaf split in two
    /// stays half full until this walks past it. A hand keeps its place
    /// between calls; once a whole pass packs nothing, every leaf but the
    /// last is too full to take its successor's first entry, and calls
    /// return `false` at once until the segment has an eighth more leaves
    /// or an eighth fewer entries than it rested with. Meant for a maintenance tick with a time budget:
    /// call it in small steps until it returns `false` or the budget runs
    /// out.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for i in 0..20_000 {
    ///     s.apply(format!("k{i}").as_bytes(), None, Some(IndexValue::I64(i * 7919 % 20_000)));
    /// }
    /// let before = s.stats().approx_bytes;
    /// while s.tidy(64) {}
    /// assert!(s.stats().approx_bytes < before);
    /// assert_eq!(s.stats().entries, 20_000);
    /// assert!(!s.tidy(64), "a packed segment rests");
    /// ```
    pub fn tidy(&mut self, leaves: usize) -> bool {
        self.tree.tidy(&mut self.tidy, leaves)
    }
}

/// The repack hand and the tree's shape at one instant, read before and
/// after a [`Segment::tidy`] step by a harness that explains slow steps:
/// the differences say how many entries moved, how many leaves and inner
/// nodes were freed, and which of the step's buffers had to grow.
#[cfg(feature = "tidy-trace")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TidyProbe {
    /// Entries in the tree.
    pub entries: usize,
    /// Live leaves.
    pub leaves: usize,
    /// Live inner nodes.
    pub inners: usize,
    /// Levels above the leaves.
    pub height: usize,
    /// Bytes held by separators.
    pub sep_bytes: usize,
    /// Bytes of out-of-line entry tails.
    pub overflow_bytes: usize,
    /// Capacity of the freed-leaf list, which grows as leaves are dropped.
    pub free_list_cap: usize,
    /// Capacity of the hand's key buffer.
    pub hand_cap: usize,
    /// Entries the current lap has moved so far.
    pub lap_moved: usize,
    /// Whether the segment is resting after a lap that moved nothing.
    pub resting: bool,
}

#[cfg(feature = "tidy-trace")]
impl Segment {
    /// The repack's state and the tree's shape now; see [`TidyProbe`].
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut s = Segment::new();
    /// for i in 0..5_000 {
    ///     s.apply(format!("k{i}").as_bytes(), None, Some(IndexValue::I64(i * 7919 % 5_000)));
    /// }
    /// let before = s.tidy_probe();
    /// while s.tidy(4) {}
    /// let after = s.tidy_probe();
    /// assert!(after.leaves < before.leaves && after.resting);
    /// assert_eq!(after.entries, 5_000);
    /// ```
    #[doc(hidden)]
    pub fn tidy_probe(&self) -> TidyProbe {
        self.tree.tidy_probe(&self.tidy)
    }
}
