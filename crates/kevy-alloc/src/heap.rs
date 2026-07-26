//! The per-shard heap.
//!
//! # Why there is no thread-local cache in front of this
//!
//! tcmalloc and mimalloc put a thread cache ahead of a shared central
//! heap because they cannot know how threads relate to memory, and
//! torajs-mmalloc's finding doc records what happens without one: its
//! first cutover cost 10–30 ns per allocation and reversed alloc-heavy
//! benchmarks by up to 4×, until a TLAB went in front.
//!
//! kevy pins a shard per core and routes every key to its owner, so the
//! heap **is** the thread-local structure. The fast path pops from the
//! current span's free list with no atomics — which is what a thread
//! cache exists to achieve. Adding one here would put a cache in front
//! of a cache. This is the divergence from the references that ROADMAP
//! rule ② asks to be stated rather than assumed.
//!
//! Cross-shard frees are real (values travel on the shared read lane),
//! and they are handled by [`segment::push_foreign`] — push-only, so
//! there is no ABA hazard to inherit.

use core::ptr::NonNull;

use crate::class::{self, NCLASSES, SPAN_BYTES};
use crate::os;
use crate::segment::{
    self, FIRST_DATA_SPAN, NO_CLASS, SEGMENT_BYTES, SPANS_PER_SEGMENT, Segment,
};
use crate::stats::Stats;

/// Spans one class may hold at once, per heap. 64 spans × 64 KiB caps a
/// class at 4 MiB.
///
/// torajs-mmalloc shipped without this and paid for it (`c2970b6d`): a
/// legal program exhausted a class, the allocator returned `None`, and
/// the null propagated into a write — a SIGSEGV on correct code. A cap
/// does not prevent exhaustion; it makes exhaustion arrive as a null
/// from `alloc`, which Rust turns into a clean abort.
pub const PER_CLASS_CAP: u16 = 64;

/// Empty spans a heap keeps mapped-but-discarded before releasing the
/// whole segment. Decay-style hysteresis, after jemalloc: releasing
/// eagerly turns a churny workload into an mmap/munmap storm.
pub const EMPTY_SPAN_HYSTERESIS: u16 = 4;

/// One shard's heap. Not `Sync`: exactly one thread owns it, which is
/// what removes the atomics from the fast path.
pub struct Heap {
    id: usize,
    segments: *mut Segment,
    /// Current span per class, as (segment, span index).
    partial: [Option<(NonNull<Segment>, u8)>; NCLASSES],
    spans_in_class: [u16; NCLASSES],
    live_bytes: u64,
    rounding_bytes: u64,
    large_mapped: u64,
    large_count: u64,
}

impl Heap {
    /// A heap owning nothing. `id` identifies the shard in stats and in
    /// segment headers.
    #[must_use]
    pub const fn new(id: usize) -> Self {
        Self {
            id,
            segments: core::ptr::null_mut(),
            partial: [None; NCLASSES],
            spans_in_class: [0; NCLASSES],
            live_bytes: 0,
            rounding_bytes: 0,
            large_mapped: 0,
            large_count: 0,
        }
    }

    /// Allocate `size` bytes aligned to `align`, or `None` if the OS or
    /// a class cap says no.
    ///
    /// Alignment up to [`class::MAX_NATIVE_ALIGN`] is served by choosing
    /// a suitable class. Stricter requests fall to the direct-mapping
    /// path, which returns page-aligned memory; anything beyond a page
    /// belongs to the `GlobalAlloc` shim's over-aligned path.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        match class::index_of(size, align) {
            Some(c) => self.alloc_small(c, size),
            None => self.alloc_large(size, align),
        }
    }

    /// Return an allocation. `size` must be the one it was made with —
    /// the sized-dealloc contract is what lets us store no headers.
    ///
    /// # Safety
    /// `ptr` must come from [`Self::alloc`] on this heap with this
    /// `size`, and must not be used afterwards.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, size: usize, align: usize) {
        match class::index_of(size, align) {
            // SAFETY: delegated to the caller's contract.
            Some(c) => unsafe { self.dealloc_small(ptr, c, size) },
            None => unsafe { self.dealloc_large(ptr, size) },
        }
    }

    fn alloc_small(&mut self, c: usize, size: usize) -> Option<NonNull<u8>> {
        let slot = self.pop_slot(c).or_else(|| self.slow_path(c))?;
        self.live_bytes += size as u64;
        self.rounding_bytes += (class::size_of(c) - size) as u64;
        Some(slot)
    }

    /// The current span had nothing. Look wider before asking the OS.
    ///
    /// The order matters, and one step here was missing at first: slots
    /// freed into a span that is *not* the current one land on that
    /// span's own free list, so without [`Self::adopt_partial`] those
    /// spans are never revisited. Allocation would keep claiming fresh
    /// spans past perfectly reusable ones until `PER_CLASS_CAP` refused
    /// — looking exactly like a leak while every byte was accounted for.
    fn slow_path(&mut self, c: usize) -> Option<NonNull<u8>> {
        self.drain_foreign();
        if self.adopt_partial(c)
            && let Some(p) = self.pop_slot(c)
        {
            return Some(p);
        }
        self.claim_span(c)?;
        self.pop_slot(c)
    }

    /// Make some span of class `c` that still has room the current one.
    fn adopt_partial(&mut self, c: usize) -> bool {
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: the list holds live segment headers only.
            let s = unsafe { &*seg };
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                let m = &s.spans[ix];
                if m.class as usize == c && (m.free_head != 0 || m.bump < m.capacity()) {
                    // SAFETY: `seg` is non-null in this branch.
                    self.partial[c] = Some((unsafe { NonNull::new_unchecked(seg) }, ix as u8));
                    return true;
                }
            }
            seg = s.next;
        }
        false
    }

    /// Take one slot from the class's current span, without falling back.
    fn pop_slot(&mut self, c: usize) -> Option<NonNull<u8>> {
        let (seg, span_ix) = self.partial[c]?;
        // SAFETY: partial entries are spans this heap assigned and has
        // not released; the segment header outlives them.
        let seg_ref = unsafe { seg.as_ref() };
        let base = seg_ref.span_base(span_ix as usize);
        // SAFETY: same — the metadata slot exists for every span index.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[span_ix as usize] };
        let slot_size = class::size_of(c);
        let out = if meta.free_head != 0 {
            let ix = (meta.free_head - 1) as usize;
            let addr = base.wrapping_add(ix * slot_size);
            // SAFETY: a free slot's first bytes hold the next index.
            meta.free_head = unsafe { addr.cast::<u32>().read() };
            addr
        } else if meta.bump < meta.capacity() {
            let addr = base.wrapping_add(meta.bump as usize * slot_size);
            meta.bump += 1;
            addr
        } else {
            self.partial[c] = None;
            return None;
        };
        meta.live += 1;
        NonNull::new(out)
    }

    /// Assign a span to class `c` and make it current, mapping a new
    /// segment if no free span exists. `None` means the cap or the OS
    /// refused.
    fn claim_span(&mut self, c: usize) -> Option<()> {
        if self.spans_in_class[c] >= PER_CLASS_CAP {
            return None;
        }
        let (seg, ix) = self.find_free_span().or_else(|| {
            self.map_segment()?;
            self.find_free_span()
        })?;
        // SAFETY: `find_free_span` returns a span of a live segment.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[ix] };
        // Pages may have been returned; the region reads as zeroes now,
        // so the cursors start over rather than trusting stale ones.
        meta.discarded = false;
        meta.class = c as u8;
        meta.free_head = 0;
        meta.live = 0;
        meta.bump = 0;
        self.spans_in_class[c] += 1;
        self.partial[c] = Some((seg, ix as u8));
        Some(())
    }

    /// First span not assigned to a class, across this heap's segments.
    fn find_free_span(&self) -> Option<(NonNull<Segment>, usize)> {
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: the list holds live segment headers only.
            let s = unsafe { &*seg };
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                if s.spans[ix].class == NO_CLASS {
                    // SAFETY: `seg` is non-null in this branch.
                    return Some((unsafe { NonNull::new_unchecked(seg) }, ix));
                }
            }
            seg = s.next;
        }
        None
    }

    /// Map a new segment and link it in. `None` when the OS refuses.
    fn map_segment(&mut self) -> Option<()> {
        let base = os::map_aligned(SEGMENT_BYTES, SEGMENT_BYTES)?;
        // SAFETY: a fresh exclusive mapping of exactly one segment.
        let seg = unsafe { Segment::init(base, self.id) };
        // SAFETY: just initialised and owned solely by this heap.
        unsafe { (*seg.as_ptr()).next = self.segments };
        self.segments = seg.as_ptr();
        Some(())
    }

    /// # Safety
    /// See [`Self::dealloc`].
    unsafe fn dealloc_small(&mut self, ptr: NonNull<u8>, c: usize, size: usize) {
        // SAFETY: a small allocation always lies inside a segment.
        let seg = unsafe { segment::segment_of(ptr) };
        // SAFETY: the mask lands on a live header for our own pointers.
        let seg_ref = unsafe { seg.as_ref() };
        debug_assert!(seg_ref.is_valid(), "pointer did not come from kevy-alloc");
        self.live_bytes -= size as u64;
        self.rounding_bytes -= (class::size_of(c) - size) as u64;
        if seg_ref.owner == self.id {
            // SAFETY: our own segment; exclusive access.
            unsafe { free_local(seg, ptr, c) };
        } else {
            // SAFETY: the slot is unreferenced from here on.
            unsafe { segment::push_foreign(seg_ref, ptr, class::size_of(c)) };
        }
    }

    fn alloc_large(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        if align > os::PAGE {
            return None;
        }
        let mapped = os::round_up(size, os::PAGE);
        let p = os::map_aligned(mapped, os::PAGE)?;
        self.large_mapped += mapped as u64;
        self.large_count += 1;
        self.live_bytes += size as u64;
        self.rounding_bytes += (mapped - size) as u64;
        Some(p)
    }

    /// # Safety
    /// See [`Self::dealloc`].
    unsafe fn dealloc_large(&mut self, ptr: NonNull<u8>, size: usize) {
        let mapped = os::round_up(size, os::PAGE);
        self.large_mapped -= mapped as u64;
        self.large_count -= 1;
        self.live_bytes -= size as u64;
        self.rounding_bytes -= (mapped - size) as u64;
        // SAFETY: delegated to the caller's contract.
        unsafe { os::unmap(ptr, mapped) };
    }

    /// Move every slot other shards freed back onto its own span's list.
    pub fn drain_foreign(&mut self) {
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &*seg };
            let mut node = segment::take_foreign(s);
            while !node.is_null() {
                // SAFETY: foreign entries are slot addresses of this
                // segment, linked through their first word.
                let next = unsafe { node.cast::<*mut u8>().read() };
                // SAFETY: non-null in this branch.
                let p = unsafe { NonNull::new_unchecked(node) };
                let ix = segment::span_index_of(p);
                // SAFETY: the span index came from the address itself.
                let cls = unsafe { (*seg).spans[ix].class };
                if cls != NO_CLASS {
                    // SAFETY: our segment, exclusive access here.
                    unsafe { free_local(NonNull::new_unchecked(seg), p, cls as usize) };
                }
                node = next;
            }
            seg = s.next;
        }
    }

    /// Return the pages of spans with nothing live, beyond the retained
    /// few. This is the property the whole experiment rests on: glibc's
    /// brk arena provably cannot do it (see the B6 finding).
    ///
    /// The retained count is per sweep rather than cumulative. A running
    /// counter looked equivalent and was not: it only ever grew, so the
    /// second sweep found it already past the threshold and returned
    /// everything, which made the hysteresis vanish after one call.
    pub fn reclaim(&mut self) {
        let mut kept: u16 = 0;
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &mut *seg };
            for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
                let meta = s.spans[ix];
                if meta.class == NO_CLASS || meta.live != 0 || meta.discarded {
                    continue;
                }
                if kept < EMPTY_SPAN_HYSTERESIS {
                    kept += 1;
                    continue;
                }
                let c = meta.class as usize;
                if self.partial[c] == Some((unsafe { NonNull::new_unchecked(seg) }, ix as u8)) {
                    self.partial[c] = None;
                }
                self.spans_in_class[c] -= 1;
                s.spans[ix] = crate::segment::SpanMeta {
                    class: NO_CLASS,
                    discarded: true,
                    free_head: 0,
                    live: 0,
                    bump: 0,
                };
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

    /// Where every mapped byte is (`bench/V5-ACCOUNTING-CONTRACT.md` §1).
    ///
    /// Walks the segments rather than maintaining seven counters on the
    /// hot path: only `live` and `rounding` depend on the requested size
    /// and must be tracked as allocations happen. Stats are read on INFO,
    /// not per operation.
    #[must_use]
    pub fn snapshot(&self) -> Stats {
        let mut st = Stats {
            live: self.live_bytes,
            rounding: self.rounding_bytes,
            mapped: self.large_mapped,
            large_count: self.large_count,
            ..Stats::default()
        };
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &*seg };
            st.mapped += SEGMENT_BYTES as u64;
            st.segment_overhead += SPAN_BYTES as u64;
            st.cache += s.foreign_bytes.load(core::sync::atomic::Ordering::Relaxed) as u64;
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

/// Push a slot onto its own span's free list.
///
/// # Safety
/// `seg` must own `ptr`, and the caller must have exclusive access.
unsafe fn free_local(seg: NonNull<Segment>, ptr: NonNull<u8>, c: usize) {
    let ix = segment::span_index_of(ptr);
    let slot = segment::slot_index_of(ptr, c);
    // SAFETY: caller holds exclusive access to this segment.
    let meta = unsafe { &mut (*seg.as_ptr()).spans[ix] };
    // SAFETY: the slot is unreferenced, so its first word can hold the
    // link to the previous head.
    unsafe { ptr.as_ptr().cast::<u32>().write(meta.free_head) };
    meta.free_head = slot + 1;
    meta.live -= 1;
}

impl Drop for Heap {
    fn drop(&mut self) {
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list; read `next` before
            // the mapping goes away.
            let next = unsafe { (*seg).next };
            // SAFETY: this heap mapped it and is the only owner.
            unsafe {
                os::unmap(NonNull::new_unchecked(seg.cast::<u8>()), SEGMENT_BYTES);
            }
            seg = next;
        }
    }
}
