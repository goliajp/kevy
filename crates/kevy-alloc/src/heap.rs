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
//! and they are handled by [`Segment::splice_foreign`](segment::Segment::splice_foreign) — push-only, so
//! there is no ABA hazard to inherit.
//!
//! # Examples
//!
//! ```
//! let mut shard = kevy_alloc::Heap::new(0);
//! let p = shard.alloc(64, 8).ok_or("no mapping")?;
//! // SAFETY: `p` came from this heap with this size and alignment.
//! unsafe { shard.dealloc(p, 64, 8) };
//! assert_eq!(shard.snapshot().live, 0);
//! # Ok::<(), &str>(())
//! ```

use core::ptr::NonNull;
use core::sync::atomic::AtomicUsize;

use crate::class::{self, NCLASSES};
use crate::os;
use crate::outbound::Outbound;
use crate::segment::{self, SEGMENT_BYTES, Segment};
use crate::spanlist::BINS;

/// The next heap identity; `0` stays "not set", so it starts at 1.
static NEXT_IDENTITY: AtomicUsize = AtomicUsize::new(1);

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
///
/// **The same lesson, second verse:** the raise above was silently
/// pinned by its own `u16` — 65,535 spans × 64 KiB is a hidden 4 GiB
/// ceiling *per class*, and the first hour-long soak found it: a
/// 3-byte-value storm filled the 16 B class and the process aborted
/// with "memory allocation of 3 bytes failed" on a box with 48 GiB
/// free. The counter is now `u32` and the guard sits at 1 TiB per
/// class — memory governance belongs to maxmemory and the tier
/// budget, never to an invisible allocator constant.
///
/// # Examples
///
/// ```
/// use kevy_alloc::{Heap, PER_CLASS_CAP, class::SPAN_BYTES};
/// // 1 TiB per class: a runaway guard, not a budget
/// assert_eq!(PER_CLASS_CAP as u64 * SPAN_BYTES as u64, 1 << 40);
/// let _tight = Heap::with_class_cap(0, PER_CLASS_CAP / 1024);
/// ```
pub const PER_CLASS_CAP: u32 = 16_777_216;

/// One shard's heap. Not `Sync`: exactly one thread owns it, which is
/// what removes the atomics from the fast path.
///
/// # Examples
///
/// ```
/// use kevy_alloc::Heap;
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(4000, 8).ok_or("no mapping")?;
/// assert_eq!(heap.snapshot().live, 4000);
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 4000, 8) };
/// # Ok::<(), &str>(())
/// ```
#[derive(Debug)]
pub struct Heap {
    id: usize,
    pub(crate) segments: *mut Segment,
    /// Current span per class, as (segment, span index).
    pub(crate) partial: [Option<(NonNull<Segment>, u8)>; NCLASSES],
    pub(crate) spans_in_class: [u32; NCLASSES],
    /// Slots the class's spans hold (claimed words included), so a span
    /// can be compared with its class's average occupancy.
    pub(crate) class_live: [u32; NCLASSES],
    pub(crate) live_bytes: u64,
    /// Foreign frees awaiting batched shipment home. The free fast path
    /// only ever appends here — the cross-core traffic all lives in the
    /// flush. See `outbound.rs` for why this shape and not tcache-style
    /// local reuse.
    pub(crate) outbound: Outbound,
    /// Per class, the heads of its spans with room, graded by occupancy
    /// (see `spanlist`).
    pub(crate) bins: [[usize; BINS]; NCLASSES],
    /// Head of the list of spans no class holds.
    pub(crate) free_spans: usize,
    /// Per-class claimed bitmap word (the far-line amortizer): up to 64
    /// slots of the current span's lowest holed word, handed out and
    /// locally recycled without touching the segment header. One header
    /// round-trip per 64 slots instead of per slot — the collection-write
    /// residual this shape exists for. `claimed & !taken` are the bits
    /// owed back to the span on retire.
    claims: [Option<Claim>; NCLASSES],
    class_cap: u32,
    /// This heap's key in the segment owner tree (`rtree`), taken when it
    /// maps its first segment; 0 until then.
    pub(crate) token: usize,
    /// Span-state totals the snapshot reads instead of walking.
    pub(crate) tally: crate::tally::Tally,
    /// The foreign-free tally every segment of this heap posts to; null
    /// until the first segment is mapped.
    pub(crate) parked: *const segment::ForeignTally,
    /// Of `spans_in_class`, the spans holding nothing live.
    pub(crate) empty_in_class: [u32; NCLASSES],
    /// Sweeps so far: the clock page ages are counted in.
    pub(crate) epoch: u32,
    /// Spans due at each sweep, by sweep number modulo the wheel size.
    pub(crate) wheel: [usize; crate::purge::WHEEL],
    /// [`PURGE_DELAY`](crate::PURGE_DELAY), unless a test sets another.
    pub(crate) purge_delay: u32,
    /// Pages handed back, counted where the discard is issued.
    #[cfg(test)]
    pub(crate) discards: u64,
}

impl Heap {
    /// A heap owning nothing. `id` identifies the shard in stats and in
    /// segment headers.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// // `const`, so a heap can sit in a static or a thread-local initializer
    /// const EMPTY: Heap = Heap::new(0);
    /// assert_eq!(EMPTY.snapshot().mapped, 0);
    /// ```
    #[must_use]
    pub const fn new(id: usize) -> Self {
        Self::with_class_cap(id, PER_CLASS_CAP)
    }

    /// A heap with a tighter per-class ceiling than [`PER_CLASS_CAP`].
    ///
    /// The default is a runaway guard set beyond any real workload,
    /// which leaves the refusal path unreachable in a test. This makes
    /// it reachable without pretending the default is smaller than it is.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::with_class_cap(0, 1);
    /// // one span of the largest class holds two slots, and the cap is one span
    /// let a = heap.alloc(32_768, 8).ok_or("no mapping")?;
    /// let b = heap.alloc(32_768, 8).ok_or("no mapping")?;
    /// assert!(heap.alloc(32_768, 8).is_none());
    /// // SAFETY: both came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(a, 32_768, 8); heap.dealloc(b, 32_768, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub const fn with_class_cap(id: usize, class_cap: u32) -> Self {
        Self {
            id,
            segments: core::ptr::null_mut(),
            partial: [None; NCLASSES],
            spans_in_class: [0; NCLASSES],
            class_live: [0; NCLASSES],
            live_bytes: 0,
            outbound: Outbound::new(),
            bins: [[0; BINS]; NCLASSES],
            free_spans: 0,
            claims: [None; NCLASSES],
            class_cap,
            token: 0,
            tally: crate::tally::Tally::NEW,
            parked: core::ptr::null(),
            empty_in_class: [0; NCLASSES],
            epoch: 0,
            wheel: [0; crate::purge::WHEEL],
            purge_delay: crate::purge::PURGE_DELAY,
            #[cfg(test)]
            discards: 0,
        }
    }

    /// Take a process-unique identity, once.
    ///
    /// Segments record their owner so a free arriving on the wrong
    /// thread can be routed home. The identity is drawn from a counter
    /// and never reused. It used to be the heap's own address, which is
    /// unique only while the heap lives — but a thread's segments
    /// outlive it (they are leaked at exit, never unmapped), and a new
    /// thread whose thread-local block landed at the same address took
    /// the dead thread's identity. It then freed the dead thread's
    /// slots as its own: its live-byte counter went below zero and it
    /// rewrote span state in segments its lists never held. `0` means
    /// "not yet set", which is why [`Heap::new`] can stay `const`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0); // 0: identity not chosen yet
    /// heap.ensure_identity(); // now an identity no other heap in the process has had
    /// let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 64, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub fn ensure_identity(&mut self) {
        if self.id == 0 {
            self.id = NEXT_IDENTITY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Allocate `size` bytes aligned to `align`, or `None` if the OS or
    /// a class cap says no.
    ///
    /// Alignment up to [`class::MAX_NATIVE_ALIGN`] is served by choosing
    /// a suitable class. Stricter requests fall to the direct-mapping
    /// path, which returns page-aligned memory; anything beyond a page
    /// belongs to the `GlobalAlloc` shim's over-aligned path.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(24, 16).ok_or("no mapping")?;
    /// assert_eq!(p.as_ptr() as usize % 16, 0);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 24, 16) };
    /// # Ok::<(), &str>(())
    /// ```
    #[inline]
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
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(100, 8).ok_or("no mapping")?;
    /// // SAFETY (all three): `p` is live from this heap, made with the old size.
    /// assert!(unsafe { heap.try_resize_in_place(p, 100, 102, 8) }); // same class
    /// assert!(!unsafe { heap.try_resize_in_place(p, 102, 500, 8) }); // must move
    /// unsafe { heap.dealloc(p, 102, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    pub unsafe fn try_resize_in_place(
        &mut self,
        ptr: NonNull<u8>,
        old_size: usize,
        new_size: usize,
        align: usize,
    ) -> bool {
        let (Some(a), Some(b)) =
            (class::index_of(old_size, align), class::index_of(new_size, align))
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
        self.live_bytes = self.live_bytes - old_size as u64 + new_size as u64;
        true
    }

    /// Return an allocation. `size` must be the one it was made with —
    /// the sized-dealloc contract is what lets us store no headers.
    ///
    /// # Safety
    /// `ptr` must come from [`Self::alloc`] on this heap with this
    /// `size`, and must not be used afterwards.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kevy_alloc::Heap;
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(1 << 20, 8).ok_or("no mapping")?; // direct mapping
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 1 << 20, 8) };
    /// assert_eq!(heap.snapshot().live, 0);
    /// # Ok::<(), &str>(())
    /// ```
    #[inline]
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, size: usize, align: usize) {
        match class::index_of(size, align) {
            // SAFETY: this fn is `unsafe`; its contract already requires that `ptr` came
            // from this heap for this `size`/`align` and is not used again. `class::index_of`
            // returning `Some(c)` means the block was served from the small path, so
            // `dealloc_small` is the matching return path.
            Some(c) => unsafe { self.dealloc_small(ptr, c, size) },
            // SAFETY: same caller contract; `None` means the size/align pair has no size
            // class, so the block came from the large path and returns to it.
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
    ///
    /// Only the claimed-word handout is inline; everything past it is out
    /// of line so the fast path carries no frame beyond its own.
    #[inline]
    pub(crate) fn alloc_small(&mut self, c: usize, size: usize) -> Option<NonNull<u8>> {
        let Some(slot) = self.pop_claimed(c) else {
            // a tail call, so nothing on the hit path needs a saved register
            return self.alloc_refill(c, size);
        };
        self.live_bytes += size as u64;
        Some(slot)
    }

    #[cold]
    #[inline(never)]
    fn alloc_refill(&mut self, c: usize, size: usize) -> Option<NonNull<u8>> {
        let slot = self.pop_slot(c).or_else(|| self.slow_path(c))?;
        self.live_bytes += size as u64;
        Some(slot)
    }

    /// The current span had nothing. Take the class's fullest span with
    /// room; failing that, collect what other shards freed and look again;
    /// failing that, claim a span from the free list (mapping a segment if
    /// none is left). Each step is O(1) — see `spanlist` for what the two
    /// segment scans that used to sit here cost.
    fn slow_path(&mut self, c: usize) -> Option<NonNull<u8>> {
        for drained in [false, true] {
            if drained {
                self.drain_foreign();
            }
            if let Some((seg, ix)) = self.take_densest(c) {
                self.partial[c] = Some((seg, ix as u8));
                if let Some(p) = self.pop_slot(c) {
                    return Some(p);
                }
            }
        }
        self.claim_span(c)?;
        self.pop_slot(c)
    }

    /// Take the lowest free slot from the class's current span, without
    /// falling back. Lowest-first is the densification property: live
    /// slots pack toward a span's low pages, so churn migrates free
    /// space upward into whole pages the reclaim sweep can return.
    ///
    /// The handout comes from the class's claimed word; only when it
    /// runs dry does the span header get touched again (one claim per
    /// 64 slots — the far-line amortizer). Every caller arrives with the
    /// claimed word already dry: `alloc_small` tried it first, and nothing
    /// on the slow path claims a word before calling here.
    fn pop_slot(&mut self, c: usize) -> Option<NonNull<u8>> {
        self.refill_claim(c)?;
        self.pop_claimed(c)
    }

    /// Assign a span to class `c` and make it current, mapping a new
    /// segment if no free span exists. `None` means the cap or the OS
    /// refused.
    fn claim_span(&mut self, c: usize) -> Option<()> {
        if self.spans_in_class[c] >= self.class_cap {
            return None;
        }
        let (seg, ix) = match self.pop_free_span() {
            Some(s) => s,
            None => {
                self.map_segment()?;
                self.pop_free_span()?
            }
        };
        // SAFETY: the free list holds spans of this heap's live segments.
        let meta = unsafe { &mut (*seg.as_ptr()).spans[ix] };
        self.tally.span_claimed(meta);
        meta.reset(c as u8);
        self.spans_in_class[c] += 1;
        self.empty_in_class[c] += 1;
        self.partial[c] = Some((seg, ix as u8));
        Some(())
    }

    /// Map a new segment, link it in and put its spans on the free list.
    /// `None` when the OS refuses.
    fn map_segment(&mut self) -> Option<()> {
        let base = os::map_aligned(SEGMENT_BYTES, SEGMENT_BYTES)?;
        if self.token == 0 {
            self.token = crate::rtree::new_token();
        }
        if !crate::rtree::set(base.as_ptr() as usize, self.token) {
            // SAFETY: our fresh mapping, not yet referenced anywhere.
            unsafe { os::unmap(base, SEGMENT_BYTES) };
            return None;
        }
        // SAFETY: a fresh exclusive mapping of exactly one segment.
        let seg = unsafe { Segment::init(base, self.id) };
        // SAFETY: just initialised and owned solely by this heap; the
        // first segment is unmapped last, only when the heap drops.
        unsafe {
            if self.parked.is_null() {
                self.parked = (*seg.as_ptr()).home;
            }
            (*seg.as_ptr()).home = self.parked;
            (*seg.as_ptr()).next = self.segments;
        }
        self.segments = seg.as_ptr();
        self.tally.segment_mapped();
        self.file_new_segment(seg);
        Some(())
    }

    #[inline(never)]
    fn alloc_large(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        crate::large::alloc(size, align)
    }

    /// # Safety
    /// See [`Self::dealloc`].
    #[inline(never)]
    unsafe fn dealloc_large(&mut self, ptr: NonNull<u8>, size: usize) {
        // SAFETY: delegated to the caller's contract.
        unsafe { crate::large::dealloc(ptr, size) };
    }
}

#[path = "heap_claims.rs"]
mod heap_claims;
#[path = "heap_defrag.rs"]
mod heap_defrag;
#[path = "heap_free.rs"]
mod heap_free;
pub(crate) use heap_claims::Claim;
