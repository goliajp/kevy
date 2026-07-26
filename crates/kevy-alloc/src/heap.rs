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

use crate::class::{self, NCLASSES};
use crate::os;
use crate::outbound::Outbound;
use crate::partials::PartialRing;
use crate::segment::{
    self, FIRST_DATA_SPAN, NO_CLASS, SEGMENT_BYTES, SPANS_PER_SEGMENT, Segment,
};

/// Spans one class may hold at once, per heap — a runaway guard, not a
/// policy. At 64 KiB a span, this bounds one class at roughly 4 GiB per
/// shard, which no correct program reaches by accident.
///
/// torajs-mmalloc shipped without any cap and paid for it (`c2970b6d`):
/// a legal program exhausted a class, the allocator returned `None`, and
/// the null propagated into a write — a SIGSEGV on correct code. What
/// actually protects a Rust program is that a null from `alloc` becomes
/// `handle_alloc_error` and a clean abort; the cap only makes runaway
/// growth arrive there sooner.
///
/// **The inherited value was 64 spans — 4 MiB a class — and it was wrong
/// by three orders of magnitude for this engine.** The standard library
/// found it on the first run that put real work through the allocator: a
/// test holding tens of thousands of buffers of one size hit the ceiling
/// and aborted with "memory allocation of 6152 bytes failed" while the
/// machine had gigabytes free. A number that is right for a JavaScript
/// runtime's object churn is not right for a data engine, and copying it
/// across was the mistake.
///
/// [`Heap::with_class_cap`] takes a tighter bound where one is wanted —
/// which is how the exhaustion path stays testable now that the default
/// is out of reach.
pub const PER_CLASS_CAP: u16 = 65_535;

/// Empty spans a heap keeps mapped-but-discarded before releasing the
/// whole segment. Decay-style hysteresis, after jemalloc: releasing
/// eagerly turns a churny workload into an mmap/munmap storm.
pub const EMPTY_SPAN_HYSTERESIS: u16 = 4;

/// One shard's heap. Not `Sync`: exactly one thread owns it, which is
/// what removes the atomics from the fast path.
pub struct Heap {
    id: usize,
    pub(crate) segments: *mut Segment,
    /// Current span per class, as (segment, span index).
    pub(crate) partial: [Option<(NonNull<Segment>, u8)>; NCLASSES],
    pub(crate) spans_in_class: [u16; NCLASSES],
    pub(crate) live_bytes: u64,
    pub(crate) rounding_bytes: u64,
    /// Foreign frees awaiting batched shipment home. The free fast path
    /// only ever appends here — the cross-core traffic all lives in the
    /// flush. See `outbound.rs` for why this shape and not tcache-style
    /// local reuse.
    pub(crate) outbound: Outbound,
    /// Per-class ring of spans believed to have room — pushed when a
    /// free makes a full span partial, popped by the slow path before
    /// it falls back to scanning.
    ///
    /// The legacy profile forced this (finding
    /// `2026-07-27-mmap-lock-was-the-killer.md`, follow-up): with the
    /// 16–32 KiB classes a span holds 2–8 slots, so churn exhausts one
    /// every few allocations, and the slow path's two O(segments)
    /// scans put `Heap::alloc` at 6 % of server self time. This is
    /// mimalloc's page-queue-per-class, sized as a ring because entries
    /// may go stale (a span can be reassigned after its entry is
    /// pushed) — the pop validates and simply discards liars.
    partials: [PartialRing; NCLASSES],
    class_cap: u16,
}

impl Heap {
    /// A heap owning nothing. `id` identifies the shard in stats and in
    /// segment headers.
    #[must_use]
    pub const fn new(id: usize) -> Self {
        Self::with_class_cap(id, PER_CLASS_CAP)
    }

    /// A heap with a tighter per-class ceiling than [`PER_CLASS_CAP`].
    ///
    /// The default is a runaway guard set beyond any real workload,
    /// which leaves the refusal path unreachable in a test. This makes
    /// it reachable without pretending the default is smaller than it is.
    #[must_use]
    pub const fn with_class_cap(id: usize, class_cap: u16) -> Self {
        Self {
            id,
            segments: core::ptr::null_mut(),
            partial: [None; NCLASSES],
            spans_in_class: [0; NCLASSES],
            live_bytes: 0,
            rounding_bytes: 0,
            outbound: Outbound::new(),
            partials: [PartialRing::EMPTY; NCLASSES],
            class_cap,
        }
    }

    /// Adopt this heap's address as its identity, once.
    ///
    /// Segments record their owner so a free arriving on the wrong
    /// thread can be routed home. The address of the heap itself is a
    /// ready-made unique identifier — no counter, no registry, and it
    /// cannot collide while the heap is alive. `0` means "not yet set",
    /// which is why [`Heap::new`] can stay `const`.
    pub fn ensure_identity(&mut self) {
        if self.id == 0 {
            self.id = core::ptr::from_mut(self) as usize;
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

    /// Grow or shrink in place when the block's size class does not
    /// change, reporting whether it worked.
    ///
    /// This is the capability a general-purpose allocator gets from its
    /// chunk headers: glibc can often extend a block where it lies
    /// instead of moving it. Without it, `GlobalAlloc`'s default
    /// `realloc` allocates, copies and frees on every growth — and a
    /// profile of pub/sub showed exactly that, with `libc realloc`
    /// visible and cheap on the system side and nothing corresponding
    /// on ours.
    ///
    /// Refuses when the block belongs to another thread. Adjusting the
    /// owner's counters from here is precisely what this design does not
    /// do, and the caller falls back to allocate-copy-free, which routes
    /// the release home correctly.
    ///
    /// # Safety
    /// `ptr` must be a live allocation from this allocator made with
    /// `old_size` and `align`.
    pub unsafe fn try_resize_in_place(
        &mut self,
        ptr: NonNull<u8>,
        old_size: usize,
        new_size: usize,
        align: usize,
    ) -> bool {
        let (Some(a), Some(b)) = (class::index_of(old_size, align), class::index_of(new_size, align))
        else {
            return false;
        };
        if a != b {
            return false;
        }
        // SAFETY: a small allocation always lies inside a segment.
        let seg = unsafe { segment::segment_of(ptr) };
        // SAFETY: the mask lands on a live header for our own pointers.
        if unsafe { seg.as_ref() }.owner != self.id {
            return false;
        }
        let slot = class::size_of(a) as u64;
        self.live_bytes = self.live_bytes - old_size as u64 + new_size as u64;
        self.rounding_bytes = self.rounding_bytes - (slot - old_size as u64)
            + (slot - new_size as u64);
        true
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

    /// Every allocation goes through the bitmap, lowest-first — there
    /// is deliberately no free-slot cache in front of it.
    ///
    /// One existed. Its premise (keeping the hot free list in
    /// heap-local memory) died with the locality hypothesis, it never
    /// won a measurable point of throughput anywhere (0.826 → 0.844,
    /// inside the band), and the M3 re-measurement convicted it of
    /// costing 137 MB of the memory result: LIFO reuse hands back the
    /// most recently freed slot regardless of position, which undoes
    /// the lowest-first densification that page-granular reclaim feeds
    /// on — resident went 1.98× → 2.38× with the cache in place. The
    /// allocator's reason to exist outranks a cache that pays nothing.
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
        // O(1) first: spans the free path registered as having room.
        // Entries can be stale — validate, discard liars.
        while let Some((seg, ix)) = self.partials[c].pop() {
            // SAFETY: rings only hold segments from this heap's list,
            // which live as long as the heap.
            let m = unsafe { &(*seg).spans[ix] };
            if m.class as usize == c && u32::from(m.live) < m.capacity() {
                // SAFETY: non-null by construction of the ring.
                self.partial[c] = Some((unsafe { NonNull::new_unchecked(seg) }, ix as u8));
                if let Some(p) = self.pop_slot(c) {
                    return Some(p);
                }
            }
        }
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
                if m.class as usize == c && u32::from(m.live) < m.capacity() {
                    // SAFETY: `seg` is non-null in this branch.
                    self.partial[c] = Some((unsafe { NonNull::new_unchecked(seg) }, ix as u8));
                    return true;
                }
            }
            seg = s.next;
        }
        false
    }

    /// Take the lowest free slot from the class's current span, without
    /// falling back. Lowest-first is the densification property: live
    /// slots pack toward a span's low pages, so churn migrates free
    /// space upward into whole pages the reclaim sweep can return.
    fn pop_slot(&mut self, c: usize) -> Option<NonNull<u8>> {
        let (seg, span_ix) = self.partial[c]?;
        // SAFETY: partial entries are spans this heap assigned and has
        // not released; the segment header outlives them.
        let seg_ref = unsafe { seg.as_ref() };
        let base = seg_ref.span_base(span_ix as usize);
        // SAFETY: same — the metadata slot exists for every span index.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[span_ix as usize] };
        let Some(i) = meta.alloc_slot() else {
            self.partial[c] = None;
            return None;
        };
        let slot_size = class::size_of(c);
        // Landing in a returned page revives it: the kernel hands back a
        // zero page on touch, and a fresh allocation owes nothing to its
        // contents. Only the bookkeeping needs to notice.
        if meta.discarded != 0 {
            let (pa, pb) = crate::pagemap::pages_of_slot(i, slot_size);
            for p in pa..=pb {
                meta.discarded &= !(1u16 << p);
            }
        }
        NonNull::new(base.wrapping_add(i as usize * slot_size))
    }

    /// Assign a span to class `c` and make it current, mapping a new
    /// segment if no free span exists. `None` means the cap or the OS
    /// refused.
    fn claim_span(&mut self, c: usize) -> Option<()> {
        if self.spans_in_class[c] >= self.class_cap {
            return None;
        }
        let (seg, ix) = self.find_free_span().or_else(|| {
            self.map_segment()?;
            self.find_free_span()
        })?;
        // SAFETY: `find_free_span` returns a span of a live segment.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[ix] };
        meta.reset(c as u8);
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

    fn alloc_large(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        crate::large::alloc(size, align)
    }

    /// # Safety
    /// See [`Self::dealloc`].
    unsafe fn dealloc_large(&mut self, ptr: NonNull<u8>, size: usize) {
        // SAFETY: delegated to the caller's contract.
        unsafe { crate::large::dealloc(ptr, size) };
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
                // SAFETY: still queued and untouched, so the size the
                // freeing thread recorded is still there.
                let requested = unsafe { segment::foreign_requested(p) };
                let ix = segment::span_index_of(p);
                // SAFETY: the span index came from the address itself.
                let cls = unsafe { (*seg).spans[ix].class };
                if cls != NO_CLASS {
                    let c = cls as usize;
                    self.live_bytes -= requested as u64;
                    self.rounding_bytes -= (class::size_of(c) - requested) as u64;
                    // SAFETY: our segment, exclusive access here.
                    unsafe { self.free_local(NonNull::new_unchecked(seg), p, c) };
                }
                node = next;
            }
            seg = s.next;
        }
    }
}

impl Heap {
    /// Mark a slot free in its span's bitmap. Nothing is written into
    /// the slot itself — that absence is what makes its pages
    /// returnable. A span going full → partial is registered in the
    /// class's partial ring so the slow path finds it in O(1).
    ///
    /// # Safety
    /// `seg` must own `ptr`, and the caller must have exclusive access.
    pub(crate) unsafe fn free_local(&mut self, seg: NonNull<Segment>, ptr: NonNull<u8>, c: usize) {
        let ix = segment::span_index_of(ptr);
        let slot = segment::slot_index_of(ptr, c);
        // SAFETY: caller holds exclusive access to this segment.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[ix] };
        let was_full = u32::from(meta.live) == meta.capacity();
        meta.free_slot(slot);
        if was_full {
            self.partials[c].push(seg.as_ptr(), ix);
        }
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
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
            // SAFETY: this heap mapped it and is the only owner.
            unsafe {
                os::unmap(NonNull::new_unchecked(seg.cast::<u8>()), SEGMENT_BYTES);
            }
            seg = next;
        }
    }
}
