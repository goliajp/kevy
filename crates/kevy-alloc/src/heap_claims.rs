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

/// One claimed word of one span, held heap-locally. `base` is the
/// span's data base, precomputed so the handout path performs no
/// segment-header access at all.
#[derive(Clone, Copy)]
pub(crate) struct Claim {
    pub(crate) seg: NonNull<Segment>,
    pub(crate) span_ix: u8,
    pub(crate) word: u8,
    pub(crate) claimed: u64,
    pub(crate) taken: u64,
    pub(crate) base: *mut u8,
}

impl Heap {
    /// Hand out the lowest available bit of the claimed word. Fully
    /// heap-local: no segment-header access on this path.
    pub(super) fn pop_claimed(&mut self, c: usize) -> Option<NonNull<u8>> {
        let cl = self.claims[c].as_mut()?;
        let avail = cl.claimed & !cl.taken;
        if avail == 0 {
            return None;
        }
        let b = avail.trailing_zeros();
        cl.taken |= 1u64 << b;
        let i = u32::from(cl.word) * 64 + b;
        NonNull::new(cl.base.wrapping_add(i as usize * class::size_of(c)))
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
        let Some((word, claimed)) = meta.claim_word() else {
            self.partial[c] = None;
            return None;
        };
        // Claimed bits may land in returned pages; a fresh allocation
        // owes nothing to its contents, only the bookkeeping notices.
        if meta.discarded != 0 {
            let slot_size = class::size_of(c);
            let lo = u32::from(word) * 64 + claimed.trailing_zeros();
            let hi = u32::from(word) * 64 + (63 - claimed.leading_zeros());
            let (pa, _) = crate::pagemap::pages_of_slot(lo, slot_size);
            let (_, pb) = crate::pagemap::pages_of_slot(hi, slot_size);
            for p in pa..=pb {
                meta.discarded &= !(1u16 << p);
            }
        }
        // SAFETY: same header liveness as above.
        let base = unsafe { seg.as_ref() }.span_base(span_ix as usize);
        self.claims[c] = Some(Claim { seg, span_ix, word, claimed, taken: 0, base });
        Some(())
    }

    /// Write a claim's unused bits back to its span. The span regains
    /// its holes and the hint walks back; a formerly-full span is
    /// findable again through `adopt_partial`'s scan (the partial ring
    /// is an optimization, not the source of truth).
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
                sum += u64::from((cl.claimed & !cl.taken).count_ones())
                    * class::size_of(c) as u64;
            }
        }
        sum
    }
}
