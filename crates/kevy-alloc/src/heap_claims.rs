//! The claimed-word layer of [`Heap`] (child module via `#[path]`, the
//! house pattern) — the far-line amortizer for small-allocation churn.
//! One segment-header round-trip claims up to 64 slots; handout and
//! same-word recycling are heap-local. Split from `heap.rs` for the
//! 500-LOC ceiling; the seam is real: everything here is the claim
//! lifecycle, nothing else in the heap touches a claim's fields.

use core::ptr::NonNull;

use crate::class::{self, NCLASSES};
use crate::segment::Segment;

use super::Heap;

/// One claimed word of one span, held heap-locally. `lo` and `len` are
/// the address range of the word's slots, clipped to the span, so that
/// neither handing a slot out nor taking one back reads a segment
/// header or works out span and slot indices.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Claim {
    pub(crate) seg: NonNull<Segment>,
    pub(crate) span_ix: u8,
    pub(crate) word: u8,
    pub(crate) claimed: u64,
    pub(crate) taken: u64,
    pub(crate) lo: *mut u8,
    pub(crate) len: usize,
}

impl Heap {
    /// Hand out the lowest available bit of the claimed word. Fully
    /// heap-local: no segment-header access on this path.
    #[inline]
    pub(super) fn pop_claimed(&mut self, c: usize) -> Option<NonNull<u8>> {
        let cl = self.claims[c].as_mut()?;
        let avail = cl.claimed & !cl.taken;
        if avail == 0 {
            return None;
        }
        let b = avail.trailing_zeros();
        cl.taken |= 1u64 << b;
        // SAFETY: a claimed bit is a slot of the claimed word, so the
        // offset stays inside `lo..lo + len`, a range of a mapped span,
        // which is never at address zero.
        Some(unsafe { NonNull::new_unchecked(cl.lo.add(b as usize * class::size_of(c))) })
    }

    /// Retire any outstanding claim, then claim the lowest holed word
    /// of the class's current span. `None` = the span is full (the
    /// slow path takes over) or there is no current span.
    pub(super) fn refill_claim(&mut self, c: usize) -> Option<()> {
        self.retire_claim(c);
        let (seg, span_ix) = self.partial[c]?;
        // SAFETY: partial entries are spans this heap assigned and has
        // not released; the segment header outlives them.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[span_ix as usize] };
        let (was_empty, old_hw) = (meta.live == 0, meta.high_water);
        let Some((word, claimed)) = meta.claim_word() else {
            self.partial[c] = None;
            return None;
        };
        self.class_live[c] += claimed.count_ones();
        let slot = class::size_of(c) as u64;
        if was_empty {
            self.tally.span_refilled(meta, old_hw, c);
            self.empty_in_class[c] -= 1;
        }
        self.tally.touched += u64::from(meta.high_water - old_hw) * slot;
        // Claimed bits may land in returned pages; a fresh allocation
        // owes nothing to its contents, only the bookkeeping notices.
        if meta.discarded != 0 {
            let gone = crate::tally::unreturn_claim(meta, word, claimed, old_hw);
            self.tally.returned -= u64::from(gone) * slot;
        }
        self.stamp_claim(seg, span_ix as usize, word, claimed);
        let first = usize::from(word) * 64;
        let len = (class::slots_per_span(c) - first).min(64) * class::size_of(c);
        // SAFETY: same header liveness as above; `first` is a slot of this
        // span because the word has a hole, so the offset stays inside it.
        let lo = unsafe { seg.as_ref().span_base(span_ix as usize).add(first * class::size_of(c)) };
        self.claims[c] = Some(Claim { seg, span_ix, word, claimed, taken: 0, lo, len });
        Some(())
    }

    /// Write a claim's unused bits back to its span. The span regains
    /// its holes and the hint walks back, and a span that is no longer
    /// current is filed by its new occupancy.
    pub(super) fn retire_claim(&mut self, c: usize) {
        let Some(cl) = self.claims[c].take() else { return };
        let unused = cl.claimed & !cl.taken;
        if unused == 0 {
            return;
        }
        // SAFETY: claims only reference spans of this heap's live
        // segments; the header outlives the claim.
        let meta = unsafe { &mut (*cl.seg.as_ptr()).spans[cl.span_ix as usize] };
        meta.retire_word(cl.word, unused);
        if meta.live == 0 {
            self.tally.span_emptied(meta, c);
            self.empty_in_class[c] += 1;
        }
        self.class_live[c] -= unused.count_ones();
        self.file_span(cl.seg, cl.span_ix as usize);
        self.note_free(cl.seg, cl.span_ix as usize);
    }

    /// Retire every class's claim — the write-back before anything
    /// that reads span occupancy as truth (reclaim's page sweep, drop).
    pub fn flush_claims(&mut self) {
        for c in 0..NCLASSES {
            self.retire_claim(c);
        }
    }

    /// Bytes sitting claimed-but-unused across every class — from the
    /// span's view they are live (the claim pins them), from the
    /// heap's they are allocatable. The snapshot folds them into
    /// `span_free` so the accounting identity balances without a
    /// flush.
    pub(crate) fn claims_unused_bytes(&self) -> u64 {
        let mut sum = 0u64;
        for (c, cl) in self.claims.iter().enumerate() {
            if let Some(cl) = cl {
                sum += u64::from((cl.claimed & !cl.taken).count_ones()) * class::size_of(c) as u64;
            }
        }
        sum
    }
}
