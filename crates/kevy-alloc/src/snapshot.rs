//! Turning a heap into a [`Stats`] snapshot.
//!
//! Split out of `heap.rs` for the file-size rule, and the seam is a real
//! one: everything here reads, nothing allocates. The engine reads it on
//! every shard tick, so it must not cost time in proportion to the heap:
//! it reads the running totals in `tally` plus one pass over the
//! classes. The walk that defines what those totals mean is kept below,
//! test-only, as the oracle they are checked against.

use core::sync::atomic::Ordering::Relaxed;

use crate::class::{self, NCLASSES};
use crate::heap::Heap;
use crate::segment::SEGMENT_BYTES;
use crate::stats::Stats;
use crate::tally::SPAN;
#[cfg(test)]
use crate::{class::SPAN_BYTES, segment::NO_CLASS};

impl Heap {
    /// Where every mapped byte is.
    ///
    /// Reads totals kept as span state changes rather than walking the
    /// segments, so the cost is fixed by the number of size classes, not
    /// by the size of the heap.
    #[must_use]
    pub fn snapshot(&self) -> Stats {
        let t = &self.tally;
        // Slots freed by another thread are still inside this heap's
        // `live`/`rounding` totals, because that thread could not reach
        // across to adjust them. Move the amount over here so every byte
        // is counted exactly once.
        let (parked, parked_live) = if self.parked.is_null() {
            (0, 0)
        } else {
            // SAFETY: set with the first segment, mapped while the heap lives.
            let p = unsafe { &*self.parked };
            (p.bytes.load(Relaxed) as u64, p.live.load(Relaxed) as u64)
        };
        // Claimed-word bits count as held span-side (they pin pages
        // exactly as live slots do), but no caller holds them: they are
        // resident, allocatable bytes, which is `span_free`.
        let mut held = 0u64;
        for c in 0..NCLASSES {
            held += u64::from(self.class_live[c]) * class::size_of(c) as u64;
        }
        let assigned = t.assigned_spans();
        Stats {
            mapped: t.segments * SEGMENT_BYTES as u64,
            live: self.live_bytes - parked_live,
            rounding: self.rounding_bytes - (parked - parked_live),
            cache: parked,
            span_free: t.touched - held - t.returned + self.claims_unused_bytes(),
            returned: t.returned_spans * SPAN + t.returned,
            virgin: t.virgin_spans * SPAN + (assigned - t.empty_spans) * SPAN - t.touched,
            hysteresis: (t.held_spans + t.empty_spans) * SPAN,
            segment_overhead: t.segments * SPAN,
            large_count: 0,
            spans_assigned: assigned,
        }
    }

    /// The definition [`Self::snapshot`] is kept equal to: every span of
    /// every segment classified from its metadata, and every foreign
    /// list walked node by node.
    #[cfg(test)]
    pub(crate) fn snapshot_walked(&self) -> Stats {
        use crate::segment::{FIRST_DATA_SPAN, SPANS_PER_SEGMENT};
        let mut st =
            Stats { live: self.live_bytes, rounding: self.rounding_bytes, ..Stats::default() };
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &*seg };
            st.mapped += SEGMENT_BYTES as u64;
            st.segment_overhead += SPAN_BYTES as u64;
            let mut node = s.foreign.load(core::sync::atomic::Ordering::Acquire);
            while !node.is_null() {
                // SAFETY: a published chain of this segment's slots; only
                // this thread, the owner, ever unlinks it.
                let p = unsafe { core::ptr::NonNull::new_unchecked(node) };
                // SAFETY: as above; the freeing thread wrote the size.
                let requested = unsafe { crate::segment::foreign_requested(p) } as u64;
                let cls = s.spans[crate::segment::span_index_of(p)].class as usize;
                let slot = class::size_of(cls) as u64;
                st.cache += slot;
                st.live -= requested;
                st.rounding -= slot - requested;
                // SAFETY: linked through the slot's first word.
                node = unsafe { node.cast::<*mut u8>().read() };
            }
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                add_span(&mut st, &s.spans[ix]);
                if s.spans[ix].class != NO_CLASS {
                    st.spans_assigned += 1;
                }
            }
            seg = s.next;
        }
        st.span_free += self.claims_unused_bytes();
        st
    }
}

/// Which bucket a span with no class belongs in. Three, not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unassigned {
    /// Carved with the segment and never claimed: mapped, never touched.
    Virgin,
    /// Emptied, retired, and its pages handed back.
    Returned,
    /// Emptied and retired, but the discard was refused — so it is still
    /// resident, and held rather than released.
    Held,
}

/// The classification, separated from the walk that applies it.
///
/// Which of the three a given machine can produce is fixed by that
/// machine: where `os::page_size_matches()` is true no span is ever
/// `Held`, and where it is false none is ever `Returned`. Deciding it in
/// a function that takes the two facts as arguments is what lets a test
/// see all three anywhere — and `Held` is the case worth seeing, because
/// it is the one where reclaim does nothing.
pub(crate) fn unassigned_bucket(retired: bool, discarded: u16) -> Unassigned {
    if !retired {
        Unassigned::Virgin
    } else if discarded == crate::pagemap::ALL_PAGES_DISCARDED {
        Unassigned::Returned
    } else {
        Unassigned::Held
    }
}

/// Fold one span's bytes into a snapshot.
///
/// A span with no class used to go wholesale into `hysteresis`, which
/// made that one number mean three opposite things: a span nobody has
/// ever claimed (never touched — `virgin`), a span emptied and given
/// back to the OS (`returned`), and a span emptied and deliberately
/// kept (`hysteresis`, the only one the name describes). The identity
/// balances whichever bucket they land in, so nothing failed; what the
/// operator got was a single figure that could not answer the question
/// it existed for. `returned` — the term page-granular reclaim was
/// built to produce — read 0 on a workload that had just emptied
/// 20,000 values, while 89 % of the map sat under `hysteresis`.
#[cfg(test)]
fn add_span(st: &mut Stats, meta: &crate::segment::SpanMeta) {
    if meta.class == NO_CLASS {
        match unassigned_bucket(meta.retired, meta.discarded) {
            Unassigned::Virgin => st.virgin += SPAN_BYTES as u64,
            Unassigned::Returned => st.returned += SPAN_BYTES as u64,
            Unassigned::Held => st.hysteresis += SPAN_BYTES as u64,
        }
        return;
    }
    if meta.live == 0 {
        // Empty but still assigned: the purge delay is holding
        // it for its class rather than retiring it. Resident and
        // deliberately kept — the contract's `hysteresis`, exactly.
        st.hysteresis += SPAN_BYTES as u64;
        return;
    }
    let slot = class::size_of(meta.class as usize) as u64;
    // live + rounding are already counted from the requested sizes; the
    // slots themselves are exactly live * slot, so only the free parts
    // are classified here. A free slot below the high-water mark is
    // `returned` when every page it overlaps has been discarded
    // (mapped, not resident) and `span_free` otherwise (touched,
    // resident). Everything at or above the mark — including the tail
    // no slot covers — was never touched: `virgin`.
    for i in 0..u32::from(meta.high_water) {
        if meta.is_live(i) {
            continue;
        }
        let (pa, pb) = crate::pagemap::pages_of_slot(i, slot as usize);
        let all_gone = (pa..=pb).all(|p| meta.discarded & (1u16 << p) != 0);
        if all_gone {
            st.returned += slot;
        } else {
            st.span_free += slot;
        }
    }
    st.virgin += SPAN_BYTES as u64 - u64::from(meta.high_water) * slot;
}

#[cfg(test)]
mod unassigned_tests {
    use super::{Unassigned, unassigned_bucket};
    use crate::pagemap::ALL_PAGES_DISCARDED;

    /// All three, including the two this machine cannot produce. They
    /// used to be one number, and the identity balanced either way —
    /// which is exactly why nothing caught it: `returned` read 0 on a
    /// workload that had handed most of its map back, while `hysteresis`
    /// — "retained rather than released" — held it.
    #[test]
    fn an_unassigned_span_is_one_of_three_things() {
        assert_eq!(unassigned_bucket(false, 0), Unassigned::Virgin);
        assert_eq!(unassigned_bucket(false, ALL_PAGES_DISCARDED), Unassigned::Virgin);
        assert_eq!(unassigned_bucket(true, ALL_PAGES_DISCARDED), Unassigned::Returned);
        assert_eq!(unassigned_bucket(true, 0), Unassigned::Held);
        // A partial discard is not a return: some of the span is still
        // resident, so the whole span is held.
        assert_eq!(unassigned_bucket(true, ALL_PAGES_DISCARDED >> 1), Unassigned::Held);
    }
}
