//! The reclaim sweep — where pages actually go back.
//!
//! Split from `heap.rs` for the file-size rule at the seam that makes
//! sense: everything here runs on the shard tick, nothing on the
//! allocation fast path. A sweep visits only the spans the purge wheel
//! names as due (see `purge`): an empty one whose pages have all aged
//! out is unassigned and discarded whole, and one that still holds live
//! slots gives back every aged-out page no live slot overlaps — the
//! page-granular structure M3 forced (RFC §5.1).

use core::ptr::NonNull;

use crate::class;
use crate::class::SPAN_BYTES;
use crate::heap::Heap;
use crate::os;
use crate::segment::Segment;

impl Heap {
    /// Return free pages to the OS — whole spans where nothing is live,
    /// and *individual pages* inside spans that still are — once each
    /// has gone unclaimed for [`PURGE_DELAY`](crate::PURGE_DELAY) sweeps.
    /// The page-granular half is v2 (RFC §5.1): M3 measured the
    /// whole-span rule returning 3 % because a span only empties when
    /// all its slots die together, while glibc works at page
    /// granularity. Now so do we.
    ///
    /// Drains foreign frees first: slots parked by other shards pin
    /// their pages exactly as live slots do, so sweeping before
    /// draining under-returns for no reason.
    ///
    /// The cost follows the spans due at this sweep and the segments
    /// that received foreign frees, not the size of the heap.
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
        // Retained large mappings age out on their own clock (see
        // `large::pool_drain`); the tick is what advances it.
        crate::large::pool_drain();
        // Ship pending foreign frees home before sweeping: they pin
        // pages on OTHER heaps' segments, and the tick is the latency
        // bound on how long a batch may sit.
        if !self.outbound.is_empty() {
            self.outbound.flush();
        }
        self.drain_foreign();
        // the frees above scheduled their spans for this sweep, so the
        // epoch moves only now
        self.epoch = self.epoch.wrapping_add(1);
        self.sweep_due(can_discard);
    }

    /// Unhook an empty span from its class and hand its pages back.
    ///
    /// Three steps that have to happen together and in this order: drop
    /// the class's cached pointer to it (a `partial` entry naming a span
    /// that has been reset would hand out slots from a span with no
    /// class), decrement the class's span count, and only then reset the
    /// metadata.
    pub(crate) fn retire_empty_span(
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
        self.empty_in_class[c] -= 1;
        self.tally.span_retired(can_discard);
        self.delist_span(seg, ix);
        s.spans[ix].reset(crate::pagemap::NO_CLASS);
        // Emptied and handed back, which is not the same unassigned as
        // never-assigned: this span's pages were touched. The snapshot
        // needs the difference to tell `returned` from `virgin`.
        s.spans[ix].retired = true;
        self.push_free_span(seg, ix);
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
            #[cfg(test)]
            {
                self.discards += 1;
            }
        }
    }

    /// [`Self::discard_free_pages`], then move the free slots whose pages
    /// all went into the running `returned` total. Returns when the span
    /// next has a page due.
    pub(crate) fn discard_and_tally(
        &mut self,
        s: &mut Segment,
        ix: usize,
        can_discard: bool,
    ) -> Option<u32> {
        let before = s.spans[ix].discarded;
        let due = self.discard_free_pages(s, ix, can_discard);
        let meta = &mut s.spans[ix];
        let fresh = meta.discarded & !before;
        #[cfg(test)]
        {
            self.discards += u64::from(fresh.count_ones());
        }
        let gained = crate::tally::returned_after_discard(meta, fresh);
        self.tally.returned += u64::from(gained) * class::size_of(meta.class as usize) as u64;
        due
    }

    /// Hand back every page of a *live* span that no live slot overlaps
    /// and no claim has touched for [`PURGE_DELAY`](crate::PURGE_DELAY)
    /// sweeps, and say when the youngest page held back ages out.
    ///
    /// The page rule, exactly: below the high-water byte (never-touched
    /// pages are already non-resident — discarding them is a wasted
    /// syscall), not already discarded, no live slot overlapping, and
    /// aged out. Contiguous runs go to the OS in one call.
    fn discard_free_pages(&self, s: &mut Segment, ix: usize, can_discard: bool) -> Option<u32> {
        // The page rule is arithmetic at `os::PAGE`, and on a system
        // whose pages are larger those ranges are not page-aligned.
        // macOS answers 0 to such a `madvise` and reclaims nothing, so
        // running on would set `discarded`, count the pages in
        // `returned`, and lower `predicted_resident()` for memory the
        // kernel still holds — the accounting would read as a success.
        // Refusing keeps the seven-term identity true about the world.
        if !can_discard {
            return None;
        }
        let base = s.span_base(ix);
        let stamps = &s.stamps[ix].0;
        let meta = &mut s.spans[ix];
        // the waiting page that ages out first: the oldest one
        let mut oldest_waiting: Option<u32> = None;
        let mut run: Option<usize> = None;
        // a trailing `None` closes the last run
        let ages = stamps.iter().map(|&t| Some(self.epoch.wrapping_sub(t)));
        for (p, age) in ages.chain([None]).enumerate() {
            match age {
                Some(age) if page_is_free(meta, p) && age > self.purge_delay => {
                    meta.discarded |= 1u16 << p;
                    run.get_or_insert(p);
                    continue;
                }
                Some(age) if page_is_free(meta, p) => {
                    oldest_waiting = oldest_waiting.max(Some(age))
                }
                _ => {}
            }
            if let Some(r0) = run.take() {
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
        oldest_waiting.map(|age| self.due_after(age))
    }
}

/// Page `p` of a live span is touched (below the high-water byte), still
/// resident, and overlapped by no live slot.
fn page_is_free(meta: &crate::pagemap::SpanMeta, p: usize) -> bool {
    let slot = class::size_of(meta.class as usize);
    meta.discarded & (1u16 << p) == 0 && p * os::PAGE < usize::from(meta.high_water) * slot && {
        let (a, b) = crate::pagemap::slots_of_page(p, slot, meta.capacity());
        !meta.range_has_live(a, b)
    }
}
