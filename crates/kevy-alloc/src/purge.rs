//! When a free page goes back to the OS: after it has gone unclaimed for
//! [`PURGE_DELAY`](crate::PURGE_DELAY) whole sweeps, and not before.
//!
//! The sweep used to hand back every free page it found, on every tick.
//! Under steady traffic that is a loss twice over: request and reply
//! buffers free their pages between ticks, the sweep discards them, and
//! the next tick's traffic faults them back in, each fault a zeroed page.
//! Two shards serving GETs over a store that allocates nothing measured
//! 0.005 faults and a third more L3 refills per operation for it.
//!
//! The rule is mimalloc's purge delay (`purge_delay`, `mi_segment_schedule_purge`):
//! a page that is reused before its delay runs out is never purged.
//! mimalloc keeps one expiry per segment and pushes it back a little on
//! each new schedule, so a page may go earlier or later than its own
//! delay; here each page carries the sweep of its last claim, and a span
//! is visited at exactly the sweep its oldest candidate page falls due.
//! What that buys, stated as bounds:
//!
//! - no page is returned while it was claimed within the last
//!   `PURGE_DELAY` sweeps;
//! - every free page, and every empty span, is returned at the first
//!   sweep after that, so once allocation stops everything free is gone
//!   `PURGE_DELAY + 1` sweeps later;
//! - so the free memory held resident is at most: every empty span with a
//!   claim in the last `PURGE_DELAY` sweeps, whole; the free pages of live
//!   spans claimed in that window; and what was freed since the last
//!   sweep. All of it lies in spans some class held during the window, so
//!   it is bounded by the sum over classes of their peak span count in the
//!   window, times 64 KiB, and it falls to nothing once allocation stops.
//!
//! A claim marks the pages of its whole word, which is all the claim path
//! knows without touching each slot; the slots it hands out lie inside.
//!
//! A span is visited through a timing wheel keyed by sweep number, so a
//! sweep touches the spans due now and nothing else. A free schedules its
//! span for the next sweep (the freed page may already be old); the visit
//! then reschedules it for the sweep at which its next waiting page ages
//! out.

use core::ptr::NonNull;

use crate::class;
use crate::os;
use crate::pagemap::PAGES_PER_SPAN;
use crate::segment::Segment;
use crate::spanlist::{link_of, pack, unpack};

use crate::heap::Heap;

/// Whole sweeps a free page must go unclaimed before [`Heap::reclaim`]
/// hands it back — mimalloc's purge delay, counted in sweeps because the
/// engine owns the clock (one sweep per 100 ms shard tick: about a second).
///
/// A page reused within the delay is never returned, and everything free
/// is returned `PURGE_DELAY + 1` sweeps after allocation stops.
///
/// # Examples
///
/// ```
/// use kevy_alloc::{Heap, PURGE_DELAY};
/// let mut heap = Heap::new(0);
/// // sixteen spans' worth of 64-byte slots, all freed again
/// let held: Vec<_> = (0..16_384).map(|_| heap.alloc(64, 8)).collect::<Option<_>>().ok_or("no mapping")?;
/// // SAFETY: each came from this heap with this size and alignment.
/// held.into_iter().for_each(|p| unsafe { heap.dealloc(p, 64, 8) });
/// for _ in 0..PURGE_DELAY {
///     heap.reclaim();
/// }
/// // still inside the delay: the spans stay with their class
/// assert_eq!(heap.snapshot().spans_assigned, 16);
/// heap.reclaim();
/// assert_eq!(heap.snapshot().spans_assigned, 0);
/// # Ok::<(), &str>(())
/// ```
pub const PURGE_DELAY: u32 = 10;

const _: () = assert!(PURGE_DELAY as usize + 2 <= WHEEL);

/// Buckets in the wheel: a due sweep is at most `PURGE_DELAY + 1` ahead,
/// so each bucket holds only the spans due at one sweep.
pub(crate) const WHEEL: usize = 16;

/// A span's place in the wheel, beside its class-list link so the free
/// path that files the span reads this on the same line.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WheelLink {
    prev: usize,
    next: usize,
    due: u32,
    queued: bool,
}

impl WheelLink {
    pub(crate) const NONE: Self = Self { prev: 0, next: 0, due: 0, queued: false };
}

/// The sweep number of each page's last claim.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stamps(pub(crate) [u32; PAGES_PER_SPAN]);

impl Stamps {
    pub(crate) const NEW: Self = Self([0; PAGES_PER_SPAN]);
}

impl Heap {
    /// A slot of span `ix` went back to its bitmap: visit the span at the
    /// next sweep, unless it is already due then.
    #[inline]
    pub(crate) fn note_free(&mut self, seg: NonNull<Segment>, ix: usize) {
        let due = self.epoch.wrapping_add(1);
        let r = pack(seg, ix);
        // SAFETY: a span of this heap's live segment.
        let w = unsafe { link_of(r) }.wheel;
        if w.queued && w.due == due {
            return;
        }
        if w.queued {
            self.wheel_unlink(r);
        }
        self.wheel_push(r, due);
    }

    /// Record that a claim of `claimed` in `word` of span `ix` made its
    /// pages current as of this sweep.
    pub(crate) fn stamp_claim(&self, seg: NonNull<Segment>, ix: usize, word: u8, claimed: u64) {
        let slot = class::size_of(
            // SAFETY: a span of this heap's live segment.
            unsafe { (*seg.as_ptr()).spans[ix].class } as usize,
        );
        let lo = u32::from(word) * 64 + claimed.trailing_zeros();
        let hi = u32::from(word) * 64 + (63 - claimed.leading_zeros());
        let (pa, _) = crate::pagemap::pages_of_slot(lo, slot);
        let (_, pb) = crate::pagemap::pages_of_slot(hi, slot);
        // SAFETY: as above; the heap is the only thread touching stamps.
        let stamps = unsafe { &mut (*seg.as_ptr()).stamps[ix].0 };
        for s in &mut stamps[pa..=pb.min(PAGES_PER_SPAN - 1)] {
            *s = self.epoch;
        }
    }

    /// Visit every span due at this sweep.
    pub(crate) fn sweep_due(&mut self, can_discard: bool) {
        let mut r = core::mem::take(&mut self.wheel[self.epoch as usize % WHEEL]);
        while r != 0 {
            // SAFETY: wheel members are spans of this heap's live segments.
            let link = unsafe { link_of(r) };
            let next = link.wheel.next;
            debug_assert_eq!(link.wheel.due, self.epoch, "a span sat in the wrong bucket");
            link.wheel = WheelLink::NONE;
            let (seg, ix) = unpack(r);
            if let Some(due) = self.purge_span(seg, ix, can_discard) {
                self.wheel_push(r, due);
            }
            r = next;
        }
    }

    /// Return what span `ix` holds that has aged out, and say when to
    /// look again, if anything is still waiting.
    fn purge_span(&mut self, seg: NonNull<Segment>, ix: usize, can_discard: bool) -> Option<u32> {
        // SAFETY: a span of this heap's live segment, exclusive here.
        let s = unsafe { &mut *seg.as_ptr() };
        let meta = &s.spans[ix];
        if meta.live != 0 {
            return self.discard_and_tally(s, ix, can_discard);
        }
        let slot = class::size_of(meta.class as usize);
        let touched = (usize::from(meta.high_water) * slot).div_ceil(os::PAGE);
        // an empty span goes back whole, so it waits for its youngest page
        let ages = s.stamps[ix].0[..touched.min(PAGES_PER_SPAN)].iter();
        let youngest = ages.map(|&t| self.epoch.wrapping_sub(t)).min();
        match youngest {
            Some(age) if age <= self.purge_delay => Some(self.due_after(age)),
            _ => {
                self.retire_empty_span(seg, s, ix, can_discard);
                None
            }
        }
    }

    /// The sweep at which a page `age` sweeps old ages out.
    pub(crate) fn due_after(&self, age: u32) -> u32 {
        self.epoch.wrapping_add(self.purge_delay + 1 - age)
    }

    fn wheel_push(&mut self, r: usize, due: u32) {
        let b = due as usize % WHEEL;
        let head = self.wheel[b];
        if head != 0 {
            // SAFETY: a wheel member is a span of this heap.
            unsafe { link_of(head) }.wheel.prev = r;
        }
        // SAFETY: the caller passes a span of this heap not in the wheel.
        unsafe { link_of(r) }.wheel = WheelLink { prev: 0, next: head, due, queued: true };
        self.wheel[b] = r;
    }

    fn wheel_unlink(&mut self, r: usize) {
        // SAFETY: the caller passes a queued span of this heap.
        let WheelLink { prev, next, due, .. } = unsafe { link_of(r) }.wheel;
        if prev == 0 {
            self.wheel[due as usize % WHEEL] = next;
        } else {
            // SAFETY: a neighbour in the same bucket is a span of this heap.
            unsafe { link_of(prev) }.wheel.next = next;
        }
        if next != 0 {
            // SAFETY: as above.
            unsafe { link_of(next) }.wheel.prev = prev;
        }
        // SAFETY: as above.
        unsafe { link_of(r) }.wheel = WheelLink::NONE;
    }
}

#[cfg(test)]
#[path = "purge_tests.rs"]
mod tests;
