//! The reclaim sweep — where pages actually go back.
//!
//! Split from `heap.rs` for the file-size rule at the seam that makes
//! sense: everything here runs on the shard tick, nothing on the
//! allocation fast path. `reclaim` walks the segments; whole spans with
//! nothing live are unassigned and discarded (v1 behaviour, with the
//! per-sweep hysteresis), and spans that still hold live slots get the
//! v2 treatment: every page no live slot overlaps is handed back
//! individually, which is the structure M3 forced (RFC §5.1).

use core::ptr::NonNull;

use crate::class;
use crate::class::SPAN_BYTES;
use crate::heap::{EMPTY_SPAN_HYSTERESIS, Heap};
use crate::os;
use crate::segment::{FIRST_DATA_SPAN, NO_CLASS, SPANS_PER_SEGMENT, Segment};

impl Heap {
    /// Return free pages to the OS — whole spans where nothing is live,
    /// and *individual pages* inside spans that still are. The second
    /// half is v2 (RFC §5.1): M3 measured the whole-span rule returning
    /// 3 % because a span only empties when all its slots die together,
    /// while glibc works at page granularity. Now so do we.
    ///
    /// Drains foreign frees first: slots parked by other shards pin
    /// their pages exactly as live slots do, so sweeping before
    /// draining under-returns for no reason.
    ///
    /// The retained count is per sweep rather than cumulative. A running
    /// counter looked equivalent and was not: it only ever grew, so the
    /// second sweep found it already past the threshold and returned
    /// everything, which made the hysteresis vanish after one call.
    pub fn reclaim(&mut self) {
        self.flush_hot();
        // Ship pending foreign frees home before sweeping: they pin
        // pages on OTHER heaps' segments, and the tick is the latency
        // bound on how long a batch may sit.
        if !self.outbound.is_empty() {
            self.outbound.flush();
        }
        self.drain_foreign();
        let mut kept: u16 = 0;
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &mut *seg };
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                if s.spans[ix].class == NO_CLASS {
                    continue;
                }
                if s.spans[ix].live != 0 {
                    Self::discard_free_pages(s, ix);
                    continue;
                }
                if kept < EMPTY_SPAN_HYSTERESIS {
                    kept += 1;
                    continue;
                }
                let c = s.spans[ix].class as usize;
                if self.partial[c] == Some((unsafe { NonNull::new_unchecked(seg) }, ix as u8)) {
                    self.partial[c] = None;
                }
                self.spans_in_class[c] -= 1;
                s.spans[ix].reset(crate::pagemap::NO_CLASS);
                let base = s.span_base(ix);
                // SAFETY: nothing is live in this span, and the range is
                // page-aligned and inside a live mapping.
                unsafe {
                    os::discard(NonNull::new_unchecked(base), SPAN_BYTES);
                }
            }
            seg = s.next;
        }
    }

    /// Empty the heap-local hot cache back into the spans' bitmaps.
    ///
    /// Cached slots keep their bits set — their pages are genuinely
    /// pinned — so a sweep that did not flush first would under-return
    /// for no reason, exactly as an undrained foreign list would.
    fn flush_hot(&mut self) {
        for c in 0..crate::class::NCLASSES {
            let slot_bytes = class::size_of(c) as u64;
            while let Some(p) = self.hot_pop(c) {
                self.cached_bytes -= slot_bytes;
                // SAFETY: a cached slot came from this heap's own
                // segments with this class, and nobody else holds it.
                unsafe { crate::heap::free_cached(p, c) };
            }
        }
    }

    /// Hand back every page of a *live* span that no live slot overlaps.
    ///
    /// The page rule, exactly: below the high-water byte (never-touched
    /// pages are already non-resident — discarding them is a wasted
    /// syscall), not already discarded, and no live slot overlapping.
    /// Contiguous runs go to the OS in one call.
    fn discard_free_pages(s: &mut Segment, ix: usize) {
        use crate::pagemap::{PAGES_PER_SPAN, slots_of_page};
        let meta = &mut s.spans[ix];
        let slot = class::size_of(meta.class as usize);
        let cap = meta.capacity();
        let hw_bytes = meta.high_water as usize * slot;
        let base = s.span_base(ix);
        let mut run: Option<usize> = None;
        for p in 0..PAGES_PER_SPAN {
            let meta = &mut s.spans[ix];
            let fresh = meta.discarded & (1u16 << p) == 0
                && p * os::PAGE < hw_bytes
                && {
                    let (a, b) = slots_of_page(p, slot, cap);
                    !meta.range_has_live(a, b)
                };
            if fresh {
                meta.discarded |= 1u16 << p;
                run.get_or_insert(p);
            } else if let Some(r0) = run.take() {
                // SAFETY: pages r0..p hold no live slot and no metadata
                // (the bitmap lives in the header — the whole point).
                unsafe {
                    os::discard(
                        NonNull::new_unchecked(base.wrapping_add(r0 * os::PAGE)),
                        (p - r0) * os::PAGE,
                    );
                }
            }
        }
        if let Some(r0) = run {
            // SAFETY: as above, through the end of the span.
            unsafe {
                os::discard(
                    NonNull::new_unchecked(base.wrapping_add(r0 * os::PAGE)),
                    (PAGES_PER_SPAN - r0) * os::PAGE,
                );
            }
        }
    }
}
