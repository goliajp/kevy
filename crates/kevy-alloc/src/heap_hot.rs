//! The hot-slot layer of [`Heap`] (child module via `#[path]`, the
//! house pattern) — the locality round's mechanism (RFC
//! 2026-08-03-v5-r1-locality).
//!
//! The measured tax: identical instruction counts, 3.06× the L1
//! misses. This allocator's frees write nothing into the slot (that
//! absence is what makes pages returnable) and its handout is
//! lowest-hole-first, so every allocation tends to land on a COLD
//! line; glibc's freelist hands back the line the program just
//! touched. This layer recovers that reuse without reopening the
//! wound the LIFO cache died of: a **bounded** stack of just-freed
//! slots, valid only for the class's CURRENT span — time-order reuse
//! locked inside a space-order bound. Every other span keeps the
//! bitmap path and its empty-then-return semantics untouched.
//!
//! A cached slot stays LIVE from its span's point of view (the
//! bitmap bit is not freed until the cache invalidates), and
//! allocatable from the heap's — the same double-view the claimed
//! word has, folded into the accounting identity the same way.

use core::ptr::NonNull;

use crate::class::{self, NCLASSES};
use crate::segment::Segment;

use super::Heap;

/// Just-freed slots of one class's current span, newest first. The
/// bound keeps the worst case tiny; overflow simply frees through
/// the ordinary bitmap path.
pub(crate) struct HotSlots {
    seg: *mut Segment,
    span_ix: u8,
    len: u8,
    slots: [*mut u8; HOT_CAP],
}

/// How many just-freed slots one class may hold. Small on purpose:
/// the win is reusing the handful of lines the last few ops touched,
/// not building a freelist.
pub(crate) const HOT_CAP: usize = 32;

impl HotSlots {
    pub(crate) const EMPTY: Self =
        Self { seg: core::ptr::null_mut(), span_ix: 0, len: 0, slots: [core::ptr::null_mut(); HOT_CAP] };
}

impl Heap {
    /// Offer a just-freed local slot to the hot layer. Accepts only
    /// slots of the class's current span (the space bound) with room
    /// in the stack; `false` = the caller frees through the bitmap.
    pub(super) fn hot_offer(
        &mut self,
        c: usize,
        seg: NonNull<Segment>,
        span_ix: usize,
        ptr: NonNull<u8>,
    ) -> bool {
        let Some((cur, cur_ix)) = self.partial[c] else { return false };
        if cur != seg || usize::from(cur_ix) != span_ix {
            return false;
        }
        let h = &mut self.hot[c];
        if h.seg != seg.as_ptr() || usize::from(h.span_ix) != span_ix {
            // The current span moved since this stack last filled:
            // return the stale entries to their own span's bitmap
            // first, then bind to the new one.
            self.hot_invalidate(c);
            let h = &mut self.hot[c];
            h.seg = seg.as_ptr();
            h.span_ix = span_ix as u8;
        }
        let h = &mut self.hot[c];
        if usize::from(h.len) == HOT_CAP {
            self.hot_stats.full += 1;
            return false;
        }
        h.slots[usize::from(h.len)] = ptr.as_ptr();
        h.len += 1;
        true
    }

    /// Pop the most recently freed slot of the class's current span —
    /// the line the program touched moments ago. `None` sends the
    /// caller to the claimed-word path.
    pub(super) fn hot_pop(&mut self, c: usize) -> Option<NonNull<u8>> {
        let h = &mut self.hot[c];
        if h.len == 0 {
            return None;
        }
        // The stack only ever binds to the span that was current when
        // its entries were freed; if the span moved on, the entries
        // belong to a non-current span now and must not be handed out
        // (the space bound) — write them back instead.
        match self.partial[c] {
            Some((cur, cur_ix))
                if cur.as_ptr() == self.hot[c].seg && cur_ix == self.hot[c].span_ix => {}
            _ => {
                self.hot_invalidate(c);
                return None;
            }
        }
        let h = &mut self.hot[c];
        h.len -= 1;
        self.hot_stats.hits += 1;
        NonNull::new(h.slots[usize::from(h.len)])
    }

    /// Return every cached slot of one class to its span's bitmap —
    /// the moment the cache's view stops being current (span change,
    /// reclaim, drop, stats that read occupancy as truth).
    pub(super) fn hot_invalidate(&mut self, c: usize) {
        let h = &mut self.hot[c];
        let (seg, span_ix, len) = (h.seg, usize::from(h.span_ix), usize::from(h.len));
        h.len = 0;
        let Some(seg) = NonNull::new(seg) else { return };
        for i in 0..len {
            let ptr = self.hot[c].slots[i];
            let slot = crate::segment::slot_index_of(
                // SAFETY: only non-null slot pointers are stored below len.
                unsafe { NonNull::new_unchecked(ptr) },
                c,
            );
            // SAFETY: cached slots reference spans of this heap's live
            // segments; the header outlives the cache.
            let meta = unsafe { &mut (*seg.as_ptr()).spans[span_ix] };
            let was_full = u32::from(meta.live) == meta.capacity();
            meta.free_slot(slot);
            if was_full {
                self.partials[c].push(seg.as_ptr(), span_ix);
            }
        }
    }

    /// Flush every class's hot stack — the companion of
    /// [`Heap::flush_claims`], for the same readers.
    pub fn flush_hot(&mut self) {
        for c in 0..NCLASSES {
            self.hot_invalidate(c);
        }
    }

    /// Bytes held by the hot stacks — live from the spans' view,
    /// allocatable from the heap's; the snapshot folds them into
    /// `span_free` exactly like the claimed word's unused bits.
    pub(crate) fn hot_unused_bytes(&self) -> u64 {
        let mut sum = 0u64;
        for (c, h) in self.hot.iter().enumerate() {
            sum += u64::from(h.len) * class::size_of(c) as u64;
        }
        sum
    }
}

/// Hit/bypass counters — the RFC's judgment gate reads these: a cold
/// mechanism with no hits is a wrong hypothesis, not a tuning knob.
#[derive(Default)]
pub struct HotStats {
    /// Allocations served straight off the stack.
    pub hits: u64,
    /// Frees turned away because the stack was full.
    pub full: u64,
}
