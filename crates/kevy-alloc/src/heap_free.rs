//! The free side of [`Heap`] (child module via `#[path]`, the house
//! pattern) — claims-first recycling, the small free's routing, the
//! local bitmap free, the foreign-free drain, and teardown. Split from
//! `heap.rs` for the 500-LOC ceiling; the seam is real: everything here
//! runs on release paths, nothing on allocation.

use core::ptr::NonNull;

use crate::class;
use crate::os;
use crate::segment::{self, NO_CLASS, SEGMENT_BYTES, Segment};

use super::Heap;

impl Heap {
    /// The claims-first free: when the pointer lands in the class's
    /// claimed word, recycle the bit without reading the segment header
    /// at all. The match itself proves ownership — a claim only ever
    /// covers this heap's own spans, and equal segment addresses mean
    /// the same segment — so the owner check would confirm what the
    /// compare already did. 99.86 % of collection-write frees take
    /// this path (branch-rate probe, hset storm), and the header read
    /// was the fast path's single foreign cache line.
    #[inline]
    fn try_free_claimed(&mut self, seg: NonNull<Segment>, ptr: NonNull<u8>, c: usize) -> bool {
        let Some(cl) = &mut self.claims[c] else { return false };
        if cl.seg != seg {
            return false;
        }
        let ix = segment::span_index_of(ptr);
        let slot = segment::slot_index_of(ptr, c);
        if usize::from(cl.span_ix) != ix || slot / 64 != u32::from(cl.word) {
            return false;
        }
        let bit = 1u64 << (slot % 64);
        if cl.taken & bit == 0 {
            return false;
        }
        cl.taken &= !bit;
        true
    }

    /// # Safety
    /// See [`Heap::dealloc`].
    pub(super) unsafe fn dealloc_small(&mut self, ptr: NonNull<u8>, c: usize, size: usize) {
        // SAFETY: a small allocation always lies inside a segment.
        let seg = unsafe { segment::segment_of(ptr) };
        if self.try_free_claimed(seg, ptr, c) {
            self.live_bytes -= size as u64;
            self.rounding_bytes -= (class::size_of(c) - size) as u64;
            return;
        }
        // SAFETY: the mask lands on a live header for our own pointers.
        let seg_ref = unsafe { seg.as_ref() };
        debug_assert!(seg_ref.is_valid(), "pointer did not come from kevy-alloc");
        if seg_ref.owner == self.id {
            self.live_bytes -= size as u64;
            self.rounding_bytes -= (class::size_of(c) - size) as u64;
            // SAFETY: our own segment; exclusive access.
            unsafe { self.free_local(seg, ptr, c) };
        } else {
            // Not ours to decrement. The bytes were counted on the
            // allocating thread's heap, and a non-atomic counter over
            // there is exactly what this design refuses to reach across
            // for — the owner settles when it drains.
            //
            // Nor ours to touch the owner's segment per-op: M1 measured
            // that bill at 18–39 % of cross-shard KV. The free lands in
            // the local outbound ring — two plain stores — and crosses
            // cores only when a whole batch ships.
            if !self.outbound.push(ptr, size, c) {
                self.outbound.flush();
                let ok = self.outbound.push(ptr, size, c);
                debug_assert!(ok, "a freshly flushed ring cannot be full");
            }
        }
    }

    /// Move every slot other shards freed back onto its own span's list.
    ///
    /// Visits only the segments a splice queued, so the cost follows the
    /// foreign frees waiting rather than the size of the heap.
    pub fn drain_foreign(&mut self) {
        if self.parked.is_null() {
            return;
        }
        // SAFETY: set with the first segment, whose header outlives
        // every segment that could have held a foreign free.
        let tally = unsafe { &*self.parked };
        let (mut live, mut bytes) = (0usize, 0usize);
        tally.drain_pending(|seg, mut node| {
            let seg = seg.as_ptr();
            while !node.is_null() {
                // SAFETY: foreign entries are slot addresses of this
                // segment, linked through their first word.
                let next = unsafe { node.cast::<*mut u8>().read() };
                // SAFETY: non-null in this branch.
                let p = unsafe { NonNull::new_unchecked(node) };
                // SAFETY: still queued and untouched, so the size the
                // freeing thread recorded is still there.
                let requested = unsafe { segment::foreign_requested(p) };
                let ix = segment::span_index_of(p);
                // SAFETY: the span index came from the address itself.
                let cls = unsafe { (*seg).spans[ix].class };
                if cls != NO_CLASS {
                    let c = cls as usize;
                    live += requested;
                    bytes += class::size_of(c);
                    self.live_bytes -= requested as u64;
                    self.rounding_bytes -= (class::size_of(c) - requested) as u64;
                    // SAFETY: our segment, exclusive access here.
                    unsafe { self.free_local(NonNull::new_unchecked(seg), p, c) };
                }
                node = next;
            }
        });
        if bytes != 0 {
            tally.settle(live, bytes);
        }
    }

    /// Mark a slot free in its span's bitmap. Nothing is written into
    /// the slot itself — that absence is what makes its pages
    /// returnable. The span moves to the list its new occupancy grades
    /// it on, so the slow path finds it in O(1).
    ///
    /// # Safety
    /// `seg` must own `ptr`, and the caller must have exclusive access.
    pub(crate) unsafe fn free_local(&mut self, seg: NonNull<Segment>, ptr: NonNull<u8>, c: usize) {
        let ix = segment::span_index_of(ptr);
        let slot = segment::slot_index_of(ptr, c);
        // A free landing inside the class's claimed word recycles the
        // bit heap-locally — no header touch at all. Collection writes
        // are exactly this shape (several short-lived small allocations
        // per op), which is where the far-line residual lived.
        if let Some(cl) = &mut self.claims[c]
            && cl.seg == seg
            && usize::from(cl.span_ix) == ix
            && slot / 64 == u32::from(cl.word)
        {
            let bit = 1u64 << (slot % 64);
            if cl.taken & bit != 0 {
                cl.taken &= !bit;
                return;
            }
        }
        // SAFETY: caller holds exclusive access to this segment.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[ix] };
        meta.free_slot(slot);
        if meta.live == 0 {
            self.tally.span_emptied(meta, c);
            self.empty_in_class[c] += 1;
        }
        self.class_live[c] -= 1;
        self.file_span(seg, ix);
        self.note_free(seg, ix);
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // Claims hold no memory of their own — the segments they point
        // into are unmapped below — but retiring them keeps the
        // debug-assert bookkeeping (live counts) honest for any
        // instrumented teardown that walks spans first.
        self.flush_claims();
        // The retention pool is process-wide and bounded, so a heap's
        // death owes it nothing — but the fuzzer's tight RSS limit
        // watches every iteration, and draining here keeps single-heap
        // lifecycles (tests, fuzz) at zero retained bytes. Its per-heap
        // ancestor forgot the equivalent and leaked a mapping per heap.
        crate::large::pool_drain();
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list; read `next` before
            // the mapping goes away.
            let next = unsafe { (*seg).next };
            crate::rtree::set(seg as usize, 0);
            // SAFETY: this heap mapped it and is the only owner.
            unsafe {
                os::unmap(NonNull::new_unchecked(seg.cast::<u8>()), SEGMENT_BYTES);
            }
            seg = next;
        }
    }
}
