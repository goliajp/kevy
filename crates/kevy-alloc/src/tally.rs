//! Running totals behind [`Heap::snapshot`](crate::Heap::snapshot).
//!
//! The snapshot used to derive every term by walking each span of each
//! segment, so reading it cost time in proportion to the heap — and the
//! engine reads it on every shard tick. These totals move only where a
//! span changes state: a segment mapped, a span claimed, emptied,
//! refilled or retired, a claim raising the high-water mark, pages
//! discarded or reused. The allocation fast path and the claimed-word
//! free are untouched; a free that writes the span's bitmap pays one
//! compare, whether it emptied the span.
//!
//! One term is not here: slot bytes held by live spans. It is
//! `class_live × slot size` summed over the classes, which the heap
//! already maintains, so the snapshot folds it in with one pass over the
//! classes rather than a counter on every free.

use crate::class::{self, SPAN_BYTES};
use crate::pagemap::{SpanMeta, pages_of_slot, slots_of_page};
use crate::segment::{FIRST_DATA_SPAN, SPANS_PER_SEGMENT};
use crate::snapshot::{Unassigned, unassigned_bucket};

/// Data spans per segment: everything but the header span.
pub(crate) const DATA_SPANS: u64 = (SPANS_PER_SEGMENT - FIRST_DATA_SPAN) as u64;

/// Span-state totals for one heap. Each field names the spans (or span
/// bytes) the walking snapshot would have put in one bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) segments: u64,
    /// Unassigned, never claimed.
    pub(crate) virgin_spans: u64,
    /// Unassigned, retired, every page discarded.
    pub(crate) returned_spans: u64,
    /// Unassigned, retired, discard refused.
    pub(crate) held_spans: u64,
    /// Assigned with nothing live: kept for the class.
    pub(crate) empty_spans: u64,
    /// `high_water × slot` over assigned spans with something live.
    pub(crate) touched: u64,
    /// Returned free-slot bytes over assigned spans with something live.
    pub(crate) returned: u64,
}

impl Tally {
    pub(crate) const NEW: Self = Self {
        segments: 0,
        virgin_spans: 0,
        returned_spans: 0,
        held_spans: 0,
        empty_spans: 0,
        touched: 0,
        returned: 0,
    };

    pub(crate) fn segment_mapped(&mut self) {
        self.segments += 1;
        self.virgin_spans += DATA_SPANS;
    }

    pub(crate) fn assigned_spans(&self) -> u64 {
        self.segments * DATA_SPANS - self.virgin_spans - self.returned_spans - self.held_spans
    }

    /// An unassigned span, read before `reset`, is about to get a class.
    pub(crate) fn span_claimed(&mut self, meta: &SpanMeta) {
        match unassigned_bucket(meta.retired, meta.discarded) {
            Unassigned::Virgin => self.virgin_spans -= 1,
            Unassigned::Returned => self.returned_spans -= 1,
            Unassigned::Held => self.held_spans -= 1,
        }
        self.empty_spans += 1;
    }

    /// An empty assigned span went back to the free list.
    pub(crate) fn span_retired(&mut self, discarded: bool) {
        self.empty_spans -= 1;
        if discarded {
            self.returned_spans += 1;
        } else {
            self.held_spans += 1;
        }
    }

    /// The span's last live slot just went. Out of the per-slot terms,
    /// into `hysteresis` whole.
    #[cold]
    pub(crate) fn span_emptied(&mut self, meta: &SpanMeta, c: usize) {
        let slot = class::size_of(c) as u64;
        self.empty_spans += 1;
        self.touched -= u64::from(meta.high_water) * slot;
        self.returned -= u64::from(meta.returned_slots) * slot;
    }

    /// The inverse of [`Self::span_emptied`], given the span's
    /// high-water mark from before the claim that refilled it.
    pub(crate) fn span_refilled(&mut self, meta: &SpanMeta, old_hw: u16, c: usize) {
        let slot = class::size_of(c) as u64;
        self.empty_spans -= 1;
        self.touched += u64::from(old_hw) * slot;
        self.returned += u64::from(meta.returned_slots) * slot;
    }
}

/// Free slots in `first..=last` below `hw` whose every page is set in
/// `discarded` — the walking snapshot's `returned` rule, over a range.
/// `was_live` says which slots count as live.
fn returned_in(
    first: u32,
    last: u32,
    hw: u16,
    slot: usize,
    discarded: u16,
    was_live: impl Fn(u32) -> bool,
) -> u16 {
    let mut n = 0;
    for i in first..(last + 1).min(u32::from(hw)) {
        if was_live(i) {
            continue;
        }
        let (pa, pb) = pages_of_slot(i, slot);
        if (pa..=pb).all(|p| discarded & (1u16 << p) != 0) {
            n += 1;
        }
    }
    n
}

/// The slots overlapping any page set in `pages` (non-zero), inclusive.
fn slot_range(pages: u16, slot: usize, cap: u32) -> (u32, u32) {
    let lo = pages.trailing_zeros() as usize;
    let hi = 15 - pages.leading_zeros() as usize;
    (slots_of_page(lo, slot, cap).0, slots_of_page(hi, slot, cap).1)
}

/// Clear the discarded bits of the pages a fresh claim landed in, and
/// return how many of the span's returned slots that un-returned.
///
/// Counted over the slots overlapping those pages only, before and
/// after, with "before" meaning the state `claim_word` found: the
/// claimed bits free and the high-water mark at `old_hw`.
pub(crate) fn unreturn_claim(meta: &mut SpanMeta, word: u8, claimed: u64, old_hw: u16) -> u16 {
    let slot = class::size_of(meta.class as usize);
    let lo = u32::from(word) * 64 + claimed.trailing_zeros();
    let hi = u32::from(word) * 64 + (63 - claimed.leading_zeros());
    let (pa, _) = pages_of_slot(lo, slot);
    let (_, pb) = pages_of_slot(hi, slot);
    let mut pages = 0u16;
    for p in pa..=pb {
        pages |= 1u16 << p;
    }
    let (first, last) = slot_range(pages, slot, meta.capacity());
    let m: &SpanMeta = meta;
    let claimed_now = |i: u32| i / 64 == u32::from(word) && claimed & (1u64 << (i % 64)) != 0;
    let before =
        returned_in(first, last, old_hw, slot, m.discarded, |i| m.is_live(i) && !claimed_now(i));
    meta.discarded &= !pages;
    let m: &SpanMeta = meta;
    let after = returned_in(first, last, m.high_water, slot, m.discarded, |i| m.is_live(i));
    let gone = before - after;
    meta.returned_slots -= gone;
    gone
}

/// Recount after the pages in `fresh` were discarded from a live span,
/// and return how many slots became returned.
pub(crate) fn returned_after_discard(meta: &mut SpanMeta, fresh: u16) -> u16 {
    if fresh == 0 {
        return 0;
    }
    let slot = class::size_of(meta.class as usize);
    let (first, last) = slot_range(fresh, slot, meta.capacity());
    let m: &SpanMeta = meta;
    let hw = m.high_water;
    let before = returned_in(first, last, hw, slot, m.discarded & !fresh, |i| m.is_live(i));
    let after = returned_in(first, last, hw, slot, m.discarded, |i| m.is_live(i));
    let gained = after - before;
    meta.returned_slots += gained;
    gained
}

/// Bytes of one span, as the tally's unit for whole-span buckets.
pub(crate) const SPAN: u64 = SPAN_BYTES as u64;

#[cfg(test)]
#[path = "tally_tests.rs"]
mod tests;
