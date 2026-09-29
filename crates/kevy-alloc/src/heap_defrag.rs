//! Whether an allocation is worth moving (child module via `#[path]`,
//! the house pattern).
//!
//! A heap cannot move what it hands out — only the owner of a pointer can
//! — but it can say which pointers a move would help, and it decides where
//! the copy lands. The rule is jemalloc's defrag hint: a slot is worth
//! moving when its span holds fewer live slots than its class does on
//! average, and the class has at least a span's worth of holes to take
//! it. Allocation fills the fullest span first, so the copy lands in a
//! denser span than the one it left, and a span drained this way goes back
//! whole at the next reclaim. Under uniform occupancy half the spans sit
//! below average, which is what lets a heap of evenly scattered holes
//! converge rather than stay as it is.

use core::ptr::NonNull;

use crate::class;
use crate::segment;

use super::Heap;

impl Heap {
    /// Whether the allocation at `ptr`, made with `size` and `align`,
    /// would leave a sparser span than the one a fresh allocation of the
    /// same shape would land in.
    ///
    /// `false` for anything that is not a small slot of this heap: a
    /// direct mapping has nowhere denser to go, and another heap's slot is
    /// that heap's to move.
    ///
    /// # Safety
    /// `ptr` must be a live allocation from this allocator made with
    /// `size` and `align`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(7);
    /// let held: Vec<_> = (0..4_000).map(|_| heap.alloc(900, 8)).collect::<Option<_>>().ok_or("no mapping")?;
    /// // free most of the first spans' slots: those spans fall below average
    /// for p in &held[..1_000] {
    ///     // SAFETY: each came from this heap with this size and alignment.
    ///     unsafe { heap.dealloc(*p, 900, 8) };
    /// }
    /// // SAFETY: live, from this heap, with this shape.
    /// assert!(unsafe { heap.should_move(held[1_010], 900, 8) }, "a sparse span's survivor moves");
    /// // SAFETY: as above.
    /// assert!(!unsafe { heap.should_move(held[3_999], 900, 8) }, "the span being filled stays");
    /// for p in &held[1_000..] {
    ///     // SAFETY: as above.
    ///     unsafe { heap.dealloc(*p, 900, 8) };
    /// }
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub unsafe fn should_move(&self, ptr: NonNull<u8>, size: usize, align: usize) -> bool {
        let Some(c) = class::index_of(size, align) else { return false };
        // SAFETY: a small allocation always lies inside a segment.
        let seg = unsafe { segment::segment_of(ptr) };
        // SAFETY: the mask lands on a live header for this allocator's pointers.
        let s = unsafe { seg.as_ref() };
        if s.owner != self.id {
            return false;
        }
        let ix = segment::span_index_of(ptr);
        if self.partial[c] == Some((seg, ix as u8)) {
            return false;
        }
        let meta = &s.spans[ix];
        let (cap, spans) = (u64::from(meta.capacity()), u64::from(self.spans_in_class[c]));
        let class_live = u64::from(self.class_live[c]);
        spans * cap >= class_live + cap && u64::from(meta.live) * spans < class_live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    #[test]
    fn moving_what_the_hint_names_packs_the_class_into_the_fewest_spans() {
        let mut heap = Heap::new(3);
        let (size, n) = (900usize, 6_000usize);
        let mut held: Vec<NonNull<u8>> = (0..n).map(|_| heap.alloc(size, 8).unwrap()).collect();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        // scatter the holes: every span loses about 30% of its slots
        held.retain(|p| {
            let drop = xorshift(&mut x) % 10 < 3;
            // SAFETY: from this heap with this shape
            drop.then(|| unsafe { heap.dealloc(*p, size, 8) }).is_none()
        });
        let c = class::index_of(size, 8).unwrap();
        let per_span = class::slots_per_span(c);
        let spans_before = heap.spans_in_class[c];
        // a reclaim between rounds, as the shard tick runs one: emptied
        // spans leave the class, which raises its average and names the
        // next sparsest spans
        for _ in 0..8 {
            heap.reclaim();
            for p in &mut held {
                // SAFETY: live, from this heap, with this shape
                if unsafe { heap.should_move(*p, size, 8) } {
                    let q = heap.alloc(size, 8).unwrap();
                    // SAFETY: both live and of `size` bytes, distinct slots
                    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), q.as_ptr(), size) };
                    // SAFETY: from this heap with this shape; replaced by `q`
                    unsafe { heap.dealloc(*p, size, 8) };
                    *p = q;
                }
            }
        }
        heap.reclaim();
        let need = held.len().div_ceil(per_span) as u32;
        let after = heap.spans_in_class[c];
        assert!(
            after <= need + 1 + u32::from(crate::heap::EMPTY_SPAN_HYSTERESIS),
            "{spans_before} spans before, {after} after, {need} needed"
        );
        assert!(heap.snapshot().balanced());
        for p in held {
            // SAFETY: as above
            unsafe { heap.dealloc(p, size, 8) };
        }
    }

    #[test]
    fn another_heaps_slot_and_a_class_without_a_spare_span_stay() {
        let (mut a, b) = (Heap::new(4), Heap::new(5));
        let held: Vec<NonNull<u8>> = (0..500).map(|_| a.alloc(900, 8).unwrap()).collect();
        // SAFETY: live, from `a`, with this shape; `b` does not own it
        assert!(!unsafe { b.should_move(held[0], 900, 8) });
        // SAFETY: from `a` with this shape
        unsafe { a.dealloc(held[3], 900, 8) };
        // one hole in the whole class: nowhere denser to go
        // SAFETY: live, from `a`
        assert!(!unsafe { a.should_move(held[4], 900, 8) });
        for (i, p) in held.into_iter().enumerate() {
            if i != 3 {
                // SAFETY: from `a` with this shape
                unsafe { a.dealloc(p, 900, 8) };
            }
        }
    }
}
