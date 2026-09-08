//! Turning a heap into a [`Stats`] snapshot.
//!
//! Split out of `heap.rs` for the file-size rule, and the seam is a real
//! one: everything here reads, nothing allocates, and it runs on an INFO
//! call rather than per operation. Only `live` and `rounding` have to be
//! maintained as allocations happen — they depend on the size a caller
//! asked for, which nothing else records. The rest is derived by walking
//! the segments when someone asks.

use crate::class;
use crate::class::SPAN_BYTES;
use crate::heap::Heap;
use crate::segment::{FIRST_DATA_SPAN, NO_CLASS, SEGMENT_BYTES, SPANS_PER_SEGMENT};
use crate::stats::Stats;

impl Heap {
    /// Where every mapped byte is (`bench/V5-ACCOUNTING-CONTRACT.md` §1).
    ///
    /// Walks the segments rather than maintaining seven counters on the
    /// hot path: only `live` and `rounding` depend on the requested size
    /// and must be tracked as allocations happen. Stats are read on INFO,
    /// not per operation.
    #[must_use]
    pub fn snapshot(&self) -> Stats {
        let mut st =
            Stats { live: self.live_bytes, rounding: self.rounding_bytes, ..Stats::default() };
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &*seg };
            st.mapped += SEGMENT_BYTES as u64;
            st.segment_overhead += SPAN_BYTES as u64;
            // Slots freed by another thread are still inside this
            // heap's `live`/`rounding` totals, because that thread could
            // not reach across to adjust them. Move the amount over here
            // so every byte is counted exactly once.
            let parked = s.foreign_bytes.load(core::sync::atomic::Ordering::Relaxed) as u64;
            let parked_live = s.foreign_live.load(core::sync::atomic::Ordering::Relaxed) as u64;
            st.cache += parked;
            st.live -= parked_live;
            st.rounding -= parked - parked_live;
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                add_span(&mut st, &s.spans[ix]);
                if s.spans[ix].class != NO_CLASS {
                    st.spans_assigned += 1;
                }
            }
            seg = s.next;
        }
        // Claimed-word bits the heap holds locally: span-side they
        // count as live (they pin pages exactly as live slots do), but
        // no caller holds them — they are resident, allocatable bytes,
        // which is the definition of `span_free`.
        st.span_free += self.claims_unused_bytes();
        st
    }
}

/// Which bucket a span with no class belongs in. Three, not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unassigned {
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
fn unassigned_bucket(retired: bool, discarded: u16) -> Unassigned {
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
        // Empty but still assigned: the per-sweep hysteresis is holding
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
