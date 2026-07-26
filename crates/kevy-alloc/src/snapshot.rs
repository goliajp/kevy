//! Turning a heap into a [`Stats`] snapshot.
//!
//! Split out of `heap.rs` for the file-size rule, and the seam is a real
//! one: everything here reads, nothing allocates, and it runs on an INFO
//! call rather than per operation. Only `live` and `rounding` have to be
//! maintained as allocations happen — they depend on the size a caller
//! asked for, which nothing else records. The rest is derived by walking
//! the segments when someone asks.

use crate::class;
use crate::segment::{FIRST_DATA_SPAN, NO_CLASS, SEGMENT_BYTES, SPANS_PER_SEGMENT};
use crate::class::SPAN_BYTES;
use crate::heap::Heap;
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
        let mut st = Stats { live: self.live_bytes, rounding: self.rounding_bytes, ..Stats::default() };
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
        st
    }
}

/// Fold one span's bytes into a snapshot.
fn add_span(st: &mut Stats, meta: &crate::segment::SpanMeta) {
    if meta.class == NO_CLASS {
        st.hysteresis += SPAN_BYTES as u64;
        return;
    }
    let slot = class::size_of(meta.class as usize) as u64;
    // live + rounding are already counted from the requested sizes; the
    // slots themselves are exactly live * slot, so only the free parts
    // are added here.
    st.span_free += u64::from(meta.touched_free()) * slot;
    st.virgin += SPAN_BYTES as u64 - u64::from(meta.bump) * slot;
}

