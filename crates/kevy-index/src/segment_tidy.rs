//! [`Segment::tidy`]: the background repack's entry point.

use crate::segment::Segment;

impl Segment {
    /// Pack neighbouring leaves, visiting at most `leaves` of them; returns
    /// whether more work remains. Writes never pack: a leaf split in two
    /// stays half full until this walks past it. A hand keeps its place
    /// between calls; once a whole pass packs nothing, every leaf but the
    /// last is too full to take its successor's first entry, and calls
    /// return `false` at once until the segment's leaves per entry have
    /// grown by an eighth. Meant for a maintenance tick with a time budget:
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
