//! Per-span occupancy — a bitmap in the segment header, one bit per slot.
//!
//! v1 tracked free slots as a LIFO list threaded *through* the slots
//! themselves. That was the structure M3 killed: a page can only be
//! returned to the OS if no metadata lives inside it, and the free
//! list's next-pointers sat in the exact pages `MADV_DONTNEED` would
//! zero — so reclaim could only ever work on whole spans, and a span
//! returns nothing until all of its (up to 157) slots die together.
//!
//! The bitmap moves every trace of occupancy into the header, which buys
//! three properties at once (RFC §5.1 v2):
//!
//! - **pages are pure data**, so any page whose overlapping slots are
//!   all free is returnable while its neighbours stay live;
//! - **lowest-first allocation densifies** — `alloc_slot` takes the
//!   lowest free bit, so live slots pack low and churn migrates free
//!   space upward into whole pages, *manufacturing* returnable pages
//!   rather than waiting for coincident deaths;
//! - **`free` writes nothing into the slot**, one line fewer touched.
//!
//! Worst case (16 B class) is 4096 slots → 512 B of bitmap; 64 spans of
//! metadata ≈ 34 KB, comfortably inside the 64 KiB header span.

use crate::class::{self, SPAN_BYTES};
use crate::os::PAGE;

/// 4 KiB pages per 64 KiB span.
pub const PAGES_PER_SPAN: usize = SPAN_BYTES / PAGE;

/// Bitmap words: enough for the smallest class (16 B → 4096 slots).
pub const BITMAP_WORDS: usize = SPAN_BYTES / 16 / 64;

/// No class assigned — the span is free for any class to take.
pub const NO_CLASS: u8 = 0xFF;

/// Per-span bookkeeping. Deliberately *not* small: the bitmap is the
/// price of page-granular reclaim, and it lives in the header span,
/// which exists to be spent on exactly this.
#[derive(Clone, Copy)]
pub struct SpanMeta {
    /// Size class this span serves, or [`NO_CLASS`].
    pub class: u8,
    /// Lowest bitmap word that may hold a zero bit — a scan cursor,
    /// maintained so lowest-first allocation is O(words-with-no-hole)
    /// rather than O(words).
    hint: u8,
    /// Slots handed out and not yet freed.
    pub live: u16,
    /// Slots at or above this index have never been handed out; their
    /// pages were never touched and are not resident.
    pub high_water: u16,
    /// Pages returned to the OS (`MADV_DONTNEED`) while the span stays
    /// assigned. Cleared per page when an allocation lands back in one.
    pub discarded: u16,
    /// Occupancy observed by the previous reclaim sweep. The sweep
    /// compares, not the free path: activity detection costs nothing on
    /// the hot path, which is why it is a sweep-side field.
    pub last_live: u16,
    /// Consecutive sweeps with unchanged occupancy, saturating at the
    /// pacing threshold. Pages return only from spans quiet this long —
    /// the decay gate of RFC 2026-08-06-v5-reclaim-pacing (candidate A).
    pub quiet: u8,
    /// One bit per slot; set = live (or parked on a foreign list, which
    /// pins the page exactly as a live slot does).
    bitmap: [u64; BITMAP_WORDS],
}

impl SpanMeta {
    pub(crate) const fn new() -> Self {
        Self {
            class: NO_CLASS,
            hint: 0,
            live: 0,
            high_water: 0,
            discarded: 0,
            last_live: 0,
            quiet: 0,
            bitmap: [0; BITMAP_WORDS],
        }
    }

    /// Assign this span to a class, forgetting everything it held.
    /// Only legal when nothing in it is live.
    pub fn reset(&mut self, class: u8) {
        debug_assert_eq!(self.live, 0, "resetting a span with live slots");
        *self = Self { class, ..Self::new() };
    }

    /// Slots this span can hold, given its class.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        if self.class == NO_CLASS {
            return 0;
        }
        class::slots_per_span(self.class as usize) as u32
    }

    /// Take the lowest free slot, or `None` when the span is full.
    pub fn alloc_slot(&mut self) -> Option<u32> {
        let n = self.capacity();
        let words = (n as usize).div_ceil(64);
        for w in (self.hint as usize)..words {
            let holes = !self.bitmap[w];
            if holes == 0 {
                continue;
            }
            let i = (w as u32) * 64 + holes.trailing_zeros();
            if i >= n {
                // Only reachable in the last word: the free bits there
                // are past the slot count, so the span is full.
                return None;
            }
            self.bitmap[w] |= 1u64 << (i % 64);
            self.hint = w as u8;
            self.live += 1;
            if i as u16 >= self.high_water {
                self.high_water = i as u16 + 1;
            }
            return Some(i);
        }
        None
    }

    /// Claim every free bit of the lowest holed word for local
    /// handout: the bits are marked live in the bitmap (a claimed bit
    /// pins its pages exactly as a live slot does, which is what makes
    /// the claim invisible to reclaim), and the caller hands them out
    /// from its own copy without touching this header again. Returns
    /// `(word_index, claimed_mask)`, or `None` when the span is full.
    ///
    /// The far-line arithmetic this exists for: one header round-trip
    /// claims up to 64 slots, so the per-allocation touch that
    /// profiled at 17.3% of collection-write self time amortizes
    /// 64:1. Position-awareness coarsens from bit to word — the claim
    /// still takes the LOWEST holed word, so densification's
    /// lowest-first semantics survive at word granularity.
    pub fn claim_word(&mut self) -> Option<(u8, u64)> {
        let n = self.capacity();
        let words = (n as usize).div_ceil(64);
        for w in (self.hint as usize)..words {
            let valid = if (w + 1) * 64 <= n as usize {
                !0u64
            } else {
                (1u64 << (n as usize - w * 64)) - 1
            };
            let holes = !self.bitmap[w] & valid;
            if holes == 0 {
                continue;
            }
            self.bitmap[w] |= holes;
            self.live += holes.count_ones() as u16;
            let hi = (w as u32) * 64 + (63 - holes.leading_zeros());
            if hi as u16 >= self.high_water {
                self.high_water = hi as u16 + 1;
            }
            self.hint = w as u8;
            return Some((w as u8, holes));
        }
        None
    }

    /// Return the bits of a claimed word that were never handed out
    /// (or were handed out and locally freed). The exact inverse of
    /// the claim's marking; the hint walks back so lowest-first
    /// allocation sees the holes again.
    pub fn retire_word(&mut self, w: u8, unused: u64) {
        debug_assert_eq!(
            self.bitmap[w as usize] & unused,
            unused,
            "retiring bits that were not claimed"
        );
        self.bitmap[w as usize] &= !unused;
        self.live -= unused.count_ones() as u16;
        if w < self.hint {
            self.hint = w;
        }
    }

    /// Mark slot `i` free.
    pub fn free_slot(&mut self, i: u32) {
        let w = (i / 64) as usize;
        let m = 1u64 << (i % 64);
        debug_assert!(self.bitmap[w] & m != 0, "double free of slot {i}");
        self.bitmap[w] &= !m;
        self.live -= 1;
        if (w as u8) < self.hint {
            self.hint = w as u8;
        }
    }

    /// Whether slot `i` is live (or parked foreign, which pins pages
    /// identically).
    #[must_use]
    pub fn is_live(&self, i: u32) -> bool {
        self.bitmap[(i / 64) as usize] & (1u64 << (i % 64)) != 0
    }

    /// Whether any slot in `first..=last` is live.
    #[must_use]
    pub fn range_has_live(&self, first: u32, last: u32) -> bool {
        let (fw, lw) = ((first / 64) as usize, (last / 64) as usize);
        for w in fw..=lw {
            let mut mask = !0u64;
            if w == fw {
                mask &= !0u64 << (first % 64);
            }
            if w == lw {
                mask &= !0u64 >> (63 - (last % 64));
            }
            if self.bitmap[w] & mask != 0 {
                return true;
            }
        }
        false
    }
}

/// The pages slot `i` of a `slot_size` class overlaps, inclusive.
#[must_use]
pub fn pages_of_slot(i: u32, slot_size: usize) -> (usize, usize) {
    let start = i as usize * slot_size;
    let end = start + slot_size - 1;
    (start / PAGE, end / PAGE)
}

/// The slots of a `slot_size` class overlapping page `p`, inclusive,
/// clamped to `nslots`.
#[must_use]
pub fn slots_of_page(p: usize, slot_size: usize, nslots: u32) -> (u32, u32) {
    let first = (p * PAGE / slot_size) as u32;
    let last = (((p + 1) * PAGE - 1) / slot_size) as u32;
    (first.min(nslots - 1), last.min(nslots - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_for(size: usize) -> SpanMeta {
        let mut m = SpanMeta::new();
        m.reset(class::index_of(size, 8).unwrap() as u8);
        m
    }

    #[test]
    fn allocation_is_lowest_first_and_exhausts_exactly() {
        let mut m = meta_for(8192);
        let cap = m.capacity();
        for expect in 0..cap {
            assert_eq!(m.alloc_slot(), Some(expect), "not lowest-first");
        }
        assert_eq!(m.alloc_slot(), None, "over-handed past capacity");
        assert_eq!(m.live as u32, cap);
    }

    #[test]
    fn a_freed_low_slot_is_taken_before_a_higher_hole() {
        let mut m = meta_for(400);
        for _ in 0..100 {
            m.alloc_slot();
        }
        m.free_slot(3);
        m.free_slot(97);
        assert_eq!(m.alloc_slot(), Some(3), "densification broken");
        assert_eq!(m.alloc_slot(), Some(97));
    }

    #[test]
    fn range_has_live_sees_across_word_boundaries() {
        let mut m = meta_for(16); // 4096 slots, many words
        for _ in 0..=130 {
            m.alloc_slot();
        }
        for i in 0..=129 {
            m.free_slot(i);
        }
        // Slot 130 is the only survivor, sitting in word 2.
        assert!(m.range_has_live(0, 200));
        assert!(m.range_has_live(130, 130));
        assert!(!m.range_has_live(0, 129));
        assert!(!m.range_has_live(131, 300));
    }

    #[test]
    fn claim_takes_the_lowest_holed_word_and_retire_reverses_it() {
        let mut m = meta_for(400); // 157 slots -> 3 words, last partial
        for _ in 0..64 {
            m.alloc_slot(); // word 0 full
        }
        let (w, mask) = m.claim_word().expect("word 1 has holes");
        assert_eq!(w, 1, "lowest holed word");
        assert_eq!(mask, !0u64, "all 64 bits were free");
        assert_eq!(m.live, 128);
        // The span-side view: word 1 is now full, allocation skips it.
        assert_eq!(m.alloc_slot(), Some(128), "next span alloc lands in word 2");
        m.free_slot(128);
        // Retire half the claim; those bits become allocatable again.
        m.retire_word(w, 0xFFFF_FFFF);
        assert_eq!(m.live, 96);
        assert_eq!(m.alloc_slot(), Some(64), "retired bit is the lowest hole");
    }

    #[test]
    fn claim_respects_the_capacity_edge() {
        let mut m = meta_for(400); // 157 slots: word 2 has 29 valid bits
        for _ in 0..128 {
            m.alloc_slot();
        }
        let (w, mask) = m.claim_word().expect("partial last word");
        assert_eq!(w, 2);
        assert_eq!(mask.count_ones(), 157 - 128, "only valid bits claimed");
        assert_eq!(m.claim_word(), None, "span exhausted");
        assert_eq!(m.live as u32, m.capacity());
    }

    #[test]
    fn page_and_slot_maps_are_inverses() {
        for size in [16usize, 400, 416, 4096, 8192] {
            let slot = class::size_of(class::index_of(size, 8).unwrap());
            let n = (SPAN_BYTES / slot) as u32;
            for p in 0..PAGES_PER_SPAN {
                let (a, b) = slots_of_page(p, slot, n);
                for i in a..=b {
                    let (pa, pb) = pages_of_slot(i, slot);
                    assert!(
                        pa <= p && p <= pb,
                        "slot {i} of {slot}B claims pages {pa}..={pb}, not {p}"
                    );
                }
            }
        }
    }
}
