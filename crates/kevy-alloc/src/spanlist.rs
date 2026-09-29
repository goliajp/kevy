//! Where a heap finds spans: per-class lists by occupancy, and the list
//! of spans no class holds.
//!
//! The slow path used to find a span with room by walking every segment,
//! behind an eight-entry ring of recent candidates. A ring that small
//! empties under steady churn, and then each refill cost two scans of
//! every segment the heap owns — a loaded shard holds hundreds, and a
//! ten-million-row load spent 62 % of its time in those scans, getting
//! slower as it grew. Every span with room is now on exactly one list,
//! found in O(1).
//!
//! The lists are graded by occupancy, and allocation takes the fullest
//! span first — jemalloc's lowest-address-slab rule and mimalloc's page
//! queues, reached from the other side: new slots land where live ones
//! already are, so sparse spans are left to drain, and a drained span is
//! one the reclaim sweep can hand back whole.
//!
//! The invariant, which every edge below keeps: a span is on its class's
//! list exactly when it has a class, is not the class's current span, and
//! is not full; a span with no class is on the free list. Links live in
//! the segment header beside the span metadata, so a data page never
//! holds any — the property page-granular return depends on.

use core::ptr::NonNull;

use crate::heap::Heap;
use crate::segment::{FIRST_DATA_SPAN, NO_CLASS, SEGMENT_BYTES, SPANS_PER_SEGMENT, Segment};

/// Occupancy grades per class: below a quarter, a half, three quarters,
/// and the rest short of full.
pub(crate) const BINS: usize = 4;
/// The list id of the free-span list.
const FREE: u8 = u8::MAX;

/// A span's place on at most one list. `prev`/`next` are packed span
/// references (segment base | span index); 0 is none, since no segment
/// sits at address 0. `list` is 0 when unlisted, `1..=BINS` for its
/// class's occupancy grade plus one, or [`FREE`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SpanLink {
    prev: usize,
    next: usize,
    list: u8,
}

impl SpanLink {
    pub(crate) const NONE: Self = Self { prev: 0, next: 0, list: 0 };
}

/// A segment's base and a span index, as one word: the base is aligned
/// to [`SEGMENT_BYTES`], so the index fits in the low bits.
#[inline]
fn pack(seg: NonNull<Segment>, ix: usize) -> usize {
    seg.as_ptr() as usize | ix
}

#[inline]
fn unpack(r: usize) -> (NonNull<Segment>, usize) {
    let base = (r & !(SEGMENT_BYTES - 1)) as *mut Segment;
    // SAFETY: only `pack` makes these words, from a non-null segment.
    (unsafe { NonNull::new_unchecked(base) }, r & (SEGMENT_BYTES - 1))
}

/// The link of the span `r` names.
///
/// # Safety
/// `r` must name a span of a segment this heap owns and has not unmapped.
#[inline]
unsafe fn link_of<'a>(r: usize) -> &'a mut SpanLink {
    let (seg, ix) = unpack(r);
    // SAFETY: the caller guarantees a live header; the heap is the only
    // thread that touches links.
    unsafe { &mut (*seg.as_ptr()).links[ix] }
}

/// The occupancy grade of a span with `live` of `cap` slots, `live < cap`.
#[inline]
pub(crate) fn bin_of(live: u32, cap: u32) -> usize {
    (live as usize * BINS / cap as usize).min(BINS - 1)
}

impl Heap {
    fn head_mut(&mut self, list: u8, class: u8) -> &mut usize {
        if list == FREE {
            &mut self.free_spans
        } else {
            &mut self.bins[class as usize][list as usize - 1]
        }
    }

    /// # Safety
    /// `r` must name an unlisted span of this heap.
    unsafe fn push_front(&mut self, r: usize, list: u8, class: u8) {
        let head = *self.head_mut(list, class);
        if head != 0 {
            // SAFETY: list members are spans of this heap.
            unsafe { link_of(head) }.prev = r;
        }
        // SAFETY: the caller's contract.
        *unsafe { link_of(r) } = SpanLink { prev: 0, next: head, list };
        *self.head_mut(list, class) = r;
    }

    /// # Safety
    /// `r` must name a listed span of this heap; `class` is the class it
    /// was listed under (ignored for the free list).
    unsafe fn unlink(&mut self, r: usize, class: u8) {
        // SAFETY: the caller's contract.
        let SpanLink { prev, next, list } = *unsafe { link_of(r) };
        if prev == 0 {
            *self.head_mut(list, class) = next;
        } else {
            // SAFETY: a neighbour on the same list is a span of this heap.
            unsafe { link_of(prev) }.next = next;
        }
        if next != 0 {
            // SAFETY: as above.
            unsafe { link_of(next) }.prev = prev;
        }
        // SAFETY: the caller's contract.
        *unsafe { link_of(r) } = SpanLink::NONE;
    }

    /// Put a classed span on the list the invariant says it belongs on,
    /// after its occupancy (or its being current) changed.
    pub(crate) fn file_span(&mut self, seg: NonNull<Segment>, ix: usize) {
        let r = pack(seg, ix);
        // SAFETY: callers pass spans of this heap's live segments.
        let meta = unsafe { &(*seg.as_ptr()).spans[ix] };
        // SAFETY: as above.
        let have = unsafe { link_of(r) }.list;
        if have == FREE || meta.class == NO_CLASS {
            return;
        }
        let (live, cap) = (u32::from(meta.live), meta.capacity());
        let current = self.partial[meta.class as usize] == Some((seg, ix as u8));
        let want = if current || live >= cap { 0 } else { bin_of(live, cap) as u8 + 1 };
        if have == want {
            return;
        }
        let class = meta.class;
        // SAFETY: `r` is listed under `class` when `have != 0`, and
        // unlisted by the time it is pushed.
        unsafe {
            if have != 0 {
                self.unlink(r, class);
            }
            if want != 0 {
                self.push_front(r, want, class);
            }
        }
    }

    /// Take class `c`'s fullest span with room off its list.
    pub(crate) fn take_densest(&mut self, c: usize) -> Option<(NonNull<Segment>, usize)> {
        for b in (0..BINS).rev() {
            let r = self.bins[c][b];
            if r != 0 {
                // SAFETY: a list head is a listed span of this heap.
                unsafe { self.unlink(r, c as u8) };
                return Some(unpack(r));
            }
        }
        None
    }

    /// Take a span no class holds, if any.
    pub(crate) fn pop_free_span(&mut self) -> Option<(NonNull<Segment>, usize)> {
        let r = self.free_spans;
        if r == 0 {
            return None;
        }
        // SAFETY: the head of the free list is a span of this heap.
        unsafe { self.unlink(r, NO_CLASS) };
        Some(unpack(r))
    }

    /// Hand a span that no longer has a class to the free list. It must
    /// have been taken off its class's list first.
    pub(crate) fn push_free_span(&mut self, seg: NonNull<Segment>, ix: usize) {
        // SAFETY: an unlisted span of this heap's live segment.
        unsafe { self.push_front(pack(seg, ix), FREE, NO_CLASS) };
    }

    /// Put every data span of a freshly mapped segment on the free list,
    /// lowest index first out.
    pub(crate) fn file_new_segment(&mut self, seg: NonNull<Segment>) {
        for ix in (FIRST_DATA_SPAN..SPANS_PER_SEGMENT).rev() {
            self.push_free_span(seg, ix);
        }
    }

    /// Take `seg`'s span `ix` off whatever list it is on.
    pub(crate) fn delist_span(&mut self, seg: NonNull<Segment>, ix: usize) {
        let r = pack(seg, ix);
        // SAFETY: a span of this heap's live segment.
        let link = *unsafe { link_of(r) };
        if link.list != 0 {
            // SAFETY: listed; its class is still set while it is on a
            // class list.
            let class = unsafe { (*seg.as_ptr()).spans[ix].class };
            // SAFETY: as above.
            unsafe { self.unlink(r, class) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::NCLASSES;

    /// Walk every list and every span; panic on any break of the
    /// invariant in the module doc.
    fn check(h: &Heap) {
        let mut listed = std::collections::HashMap::new();
        let mut walk = |head: usize, list: u8, class: Option<usize>| {
            let (mut r, mut prev) = (head, 0usize);
            while r != 0 {
                // SAFETY: list members are spans of `h`'s live segments.
                let l = *unsafe { link_of(r) };
                assert_eq!(l.prev, prev, "back link");
                assert_eq!(l.list, list, "list id");
                if let Some(c) = class {
                    let (seg, ix) = unpack(r);
                    // SAFETY: as above.
                    assert_eq!(usize::from(unsafe { (*seg.as_ptr()).spans[ix].class }), c);
                }
                assert!(listed.insert(r, list).is_none(), "a span on two lists");
                (prev, r) = (r, l.next);
            }
        };
        for c in 0..NCLASSES {
            for b in 0..BINS {
                walk(h.bins[c][b], b as u8 + 1, Some(c));
            }
        }
        walk(h.free_spans, FREE, None);
        let mut seg = h.segments;
        while !seg.is_null() {
            // SAFETY: the heap's own segment list.
            let s = unsafe { &*seg };
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                // SAFETY: `seg` is non-null here.
                let r = pack(unsafe { NonNull::new_unchecked(seg) }, ix);
                let m = &s.spans[ix];
                let want = if m.class == NO_CLASS {
                    FREE
                } else if h.partial[m.class as usize].is_some_and(|(p, i)| pack(p, i.into()) == r)
                    || u32::from(m.live) >= m.capacity()
                {
                    0
                } else {
                    bin_of(m.live.into(), m.capacity()) as u8 + 1
                };
                assert_eq!(listed.get(&r).copied().unwrap_or(0), want, "span {ix}: {m:?}");
            }
            seg = s.next;
        }
    }

    #[test]
    fn churn_across_classes_and_heaps_keeps_every_span_on_its_list() {
        let (mut a, mut b) = (Heap::new(1), Heap::new(2));
        let sizes = [24usize, 400, 912, 4_000, 20_000];
        let mut held: Vec<(core::ptr::NonNull<u8>, usize)> = Vec::new();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for step in 0..20_000 {
            let r = next();
            match r % 10 {
                0..=4 => {
                    let size = sizes[(r >> 8) as usize % sizes.len()];
                    held.push((a.alloc(size, 8).expect("mapped"), size));
                }
                5..=7 if !held.is_empty() => {
                    let (p, size) = held.swap_remove((r >> 8) as usize % held.len());
                    // SAFETY: allocated by `a` with this size and alignment
                    unsafe { a.dealloc(p, size, 8) };
                }
                8 if !held.is_empty() => {
                    let (p, size) = held.swap_remove((r >> 8) as usize % held.len());
                    // SAFETY: as above; `b` ships it home
                    unsafe { b.dealloc(p, size, 8) };
                }
                _ => match (r >> 8) % 3 {
                    0 => a.drain_foreign(),
                    1 => b.reclaim(),
                    _ => a.reclaim(),
                },
            }
            if step % 7 == 0 {
                check(&a);
            }
        }
        for (p, size) in held.drain(..) {
            // SAFETY: as above
            unsafe { a.dealloc(p, size, 8) };
        }
        b.reclaim();
        a.reclaim();
        check(&a);
        assert_eq!(a.snapshot().live, 0);
    }

    #[test]
    fn the_grades_split_occupancy_in_quarters() {
        assert_eq!(bin_of(0, 100), 0);
        assert_eq!(bin_of(24, 100), 0);
        assert_eq!(bin_of(25, 100), 1);
        assert_eq!(bin_of(74, 100), 2);
        assert_eq!(bin_of(99, 100), 3);
        assert_eq!(bin_of(1, 2), 2);
    }
}
