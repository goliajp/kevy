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
        self.reclaim_with(os::page_size_matches());
    }

    /// [`Self::reclaim`] with the platform's answer supplied.
    ///
    /// Only one value of `can_discard` is possible on any given machine,
    /// so the other side of every branch below is unreachable where it
    /// runs — and the unreachable one is the interesting one: it is the
    /// case in which page-granular reclaim does nothing at all, which is
    /// every Apple Silicon Mac. Taking it as an argument is what lets a
    /// test drive both, and it is asked once per sweep rather than once
    /// per span.
    pub(crate) fn reclaim_with(&mut self, can_discard: bool) {
        // Claimed-word bits pin their pages exactly as live slots do;
        // write them back first so the sweep sees true occupancy.
        self.flush_claims();
        // Retained large mappings go back each tick: retention beyond a
        // tick requires sustained traffic to re-earn, and idle memory
        // stays bounded by the tick length rather than the pool size.
        crate::large::pool_drain();
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
                    Self::discard_free_pages(s, ix, can_discard);
                    continue;
                }
                if kept < EMPTY_SPAN_HYSTERESIS {
                    kept += 1;
                    continue;
                }
                // SAFETY: `seg` is the segment being walked in this loop; it came
                // from the live span list, so it is a real segment address and
                // never null.
                self.retire_empty_span(unsafe { NonNull::new_unchecked(seg) }, s, ix, can_discard);
            }
            seg = s.next;
        }
    }

    /// Unhook an empty span from its class and hand its pages back.
    ///
    /// Three steps that have to happen together and in this order: drop
    /// the class's cached pointer to it (a `partial` entry naming a span
    /// that has been reset would hand out slots from a span with no
    /// class), decrement the class's span count, and only then reset the
    /// metadata.
    fn retire_empty_span(
        &mut self,
        seg: NonNull<Segment>,
        s: &mut Segment,
        ix: usize,
        can_discard: bool,
    ) {
        let c = s.spans[ix].class as usize;
        if self.partial[c] == Some((seg, ix as u8)) {
            self.partial[c] = None;
        }
        self.spans_in_class[c] -= 1;
        s.spans[ix].reset(crate::pagemap::NO_CLASS);
        // Emptied and handed back, which is not the same unassigned as
        // never-assigned: this span's pages were touched. The snapshot
        // needs the difference to tell `returned` from `virgin`.
        s.spans[ix].retired = true;
        // Refused on a system whose page size is not `os::PAGE`: see
        // `discard_free_pages` for why reporting a return that did not
        // happen is worse than not returning. The span then stays
        // resident and is accounted as `hysteresis`, which is what it
        // is — held, not released.
        if can_discard {
            let base = s.span_base(ix);
            // SAFETY: nothing is live in this span, and the range is
            // page-aligned and inside a live mapping.
            unsafe {
                os::discard(NonNull::new_unchecked(base), SPAN_BYTES);
            }
            s.spans[ix].discarded = crate::pagemap::ALL_PAGES_DISCARDED;
        }
    }

    /// Hand back every page of a *live* span that no live slot overlaps.
    ///
    /// The page rule, exactly: below the high-water byte (never-touched
    /// pages are already non-resident — discarding them is a wasted
    /// syscall), not already discarded, and no live slot overlapping.
    /// Contiguous runs go to the OS in one call.
    fn discard_free_pages(s: &mut Segment, ix: usize, can_discard: bool) {
        use crate::pagemap::{PAGES_PER_SPAN, slots_of_page};
        // The page rule is arithmetic at `os::PAGE`, and on a system
        // whose pages are larger those ranges are not page-aligned.
        // macOS answers 0 to such a `madvise` and reclaims nothing, so
        // running on would set `discarded`, count the pages in
        // `returned`, and lower `predicted_resident()` for memory the
        // kernel still holds — the accounting would read as a success.
        // Refusing keeps the seven-term identity true about the world.
        if !can_discard {
            return;
        }
        let meta = &mut s.spans[ix];
        let slot = class::size_of(meta.class as usize);
        let cap = meta.capacity();
        let hw_bytes = meta.high_water as usize * slot;
        let base = s.span_base(ix);
        let mut run: Option<usize> = None;
        for p in 0..PAGES_PER_SPAN {
            let meta = &mut s.spans[ix];
            let fresh = meta.discarded & (1u16 << p) == 0 && p * os::PAGE < hw_bytes && {
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
