//! The foreign-free side of [`Segment`] (child module via `#[path]`, the
//! house pattern): the push-only stack other shards splice freed slots
//! onto, and its accounting. Split from `segment.rs` for the 500-LOC
//! ceiling; everything here is cross-thread, nothing else in the segment
//! is.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use super::Segment;

/// Slot bytes other threads have freed onto a heap's segments and the
/// owner has not drained yet. Posted by the freeing thread before its
/// chain is published and taken back by the owner for exactly what it
/// drained, so it never undercounts a published chain.
///
/// `AtomicUsize` rather than `AtomicU64` because 32-bit targets
/// (Cortex-M among them) have no 64-bit atomic, and pending foreign
/// frees cannot exceed the address space anyway.
#[derive(Debug)]
pub(crate) struct ForeignTally {
    /// Slot bytes.
    pub(crate) bytes: AtomicUsize,
    /// Of those, the bytes callers asked for. The owner's `live` and
    /// `rounding` still include them, because the freeing thread cannot
    /// touch the owner's counters; the snapshot moves them across.
    pub(crate) live: AtomicUsize,
    /// The heap's segments with foreign frees waiting, as a push-only
    /// stack linked through `Segment::queued_next`, so a drain visits
    /// those and not every segment the heap owns.
    pending: AtomicPtr<Segment>,
}

impl ForeignTally {
    pub(crate) const fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            pending: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// The owner drained a batch worth these sums.
    pub(crate) fn settle(&self, live: usize, bytes: usize) {
        self.live.fetch_sub(live, Ordering::Relaxed);
        self.bytes.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Take every queued segment's foreign list and call `f` with it.
    ///
    /// The flag is cleared before the list is taken, and a splice
    /// publishes its chain before it reads the flag, all sequentially
    /// consistent: either the splice sees the flag clear and queues the
    /// segment again, or the take below sees its chain. Relaxed, a
    /// splice could read a stale set flag after the take, and its chain
    /// would sit unseen until some later splice to the same segment.
    pub(crate) fn drain_pending(&self, mut f: impl FnMut(NonNull<Segment>, *mut u8)) {
        let mut seg = self.pending.swap(core::ptr::null_mut(), Ordering::Acquire);
        while !seg.is_null() {
            // SAFETY: only this heap's live segments are queued here.
            let s = unsafe { &*seg };
            // read before the flag clears: from then on a splice may
            // queue the segment again and rewrite the link
            let next = s.queued_next.load(Ordering::Relaxed);
            s.queued.store(false, Ordering::SeqCst);
            let chain = s.foreign.swap(core::ptr::null_mut(), Ordering::SeqCst);
            // SAFETY: non-null in this loop.
            f(unsafe { NonNull::new_unchecked(seg) }, chain);
            seg = next;
        }
    }

    fn push_pending(&self, seg: &Segment) {
        let me = core::ptr::from_ref(seg).cast_mut();
        let mut head = self.pending.load(Ordering::Relaxed);
        loop {
            seg.queued_next.store(head, Ordering::Relaxed);
            match self.pending.compare_exchange_weak(head, me, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }
}

impl Segment {
    /// Slot bytes other threads have freed onto this segment's heap —
    /// this segment or any other it owns — and the owner has not drained
    /// yet. A relaxed read: under concurrent frees it is a moment's
    /// value, not a bound.
    ///
    /// ```
    /// # use kevy_alloc::{Heap, segment};
    /// let mut heap = Heap::new(0);
    /// if let Some(p) = heap.alloc(64, 8) {
    ///     // SAFETY: `p` is a small slot this heap handed out.
    ///     let seg = unsafe { segment::segment_of(p).as_ref() };
    ///     assert_eq!(seg.foreign_bytes(), 0, "only the owner has freed here");
    ///     // SAFETY: allocated just above with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// ```
    #[must_use]
    pub fn foreign_bytes(&self) -> usize {
        // SAFETY: `home` is this segment's own tally or the heap's first
        // segment's, which stays mapped as long as any of its segments.
        unsafe { &*self.home }.bytes.load(Ordering::Relaxed)
    }

    /// Of [`Segment::foreign_bytes`], the bytes callers actually asked
    /// for. A relaxed read, like that one.
    ///
    /// ```
    /// # use kevy_alloc::{Heap, segment};
    /// let mut heap = Heap::new(0);
    /// if let Some(p) = heap.alloc(64, 8) {
    ///     // SAFETY: `p` is a small slot this heap handed out.
    ///     let seg = unsafe { segment::segment_of(p).as_ref() };
    ///     assert!(seg.foreign_live() <= seg.foreign_bytes());
    ///     // SAFETY: allocated just above with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// ```
    #[must_use]
    pub fn foreign_live(&self) -> usize {
        // SAFETY: as in `foreign_bytes`.
        unsafe { &*self.home }.live.load(Ordering::Relaxed)
    }

    /// Splice a pre-linked chain of freed slots onto this segment's
    /// foreign list, and post the batch's byte sums. One CAS and two
    /// `fetch_add`s for the whole chain — this is the amortisation M1
    /// forced: the per-op version of this function was three atomic RMWs
    /// on this same line for every single foreign free, and cross-shard KV
    /// paid 18–39 % for it. Then one load of the segment's queued flag,
    /// and only when the owner has drained the segment since, a swap and
    /// a push onto its heap's stack of segments to drain.
    ///
    /// The chain format is unchanged from the per-op era: each slot's
    /// first word links to the next, with the requested size at
    /// [`FOREIGN_SIZE_OFFSET`](crate::segment::FOREIGN_SIZE_OFFSET) — the owner's drain cannot tell a spliced
    /// batch from a thousand individual pushes.
    ///
    /// # Why this is push-only, and why that matters
    ///
    /// A Treiber stack's ABA hazard lives in `pop`: a consumer reads
    /// `head.next`, and between that read and its compare-and-swap another
    /// thread can pop, push other nodes, and push the same address back —
    /// so the CAS succeeds against a stale `next`. torajs-mmalloc documents
    /// the hazard and accepts it, reasoning that its runtime is
    /// single-threaded. kevy is not: values are shared across shards on the
    /// read lane, so a foreign free is ordinary, and inheriting that note
    /// would be inheriting a bug.
    ///
    /// The fix is structural rather than defensive. **Only the owning shard
    /// ever removes anything, and it removes the entire list with one
    /// `swap`** ([`Segment::take_foreign`]). There is no compare-and-swap
    /// on the consumer side, so there is no window for ABA to open.
    /// Producers only ever push. This is mimalloc's thread-free design, and
    /// it is strictly simpler than tagged pointers or hazard pointers would
    /// have been.
    ///
    /// # Safety
    /// `head..tail` must be a chain of live slot addresses belonging to
    /// this segment, linked through their first words, referenced by
    /// nobody else; `live_sum`/`bytes_sum` must be the chain's
    /// requested/slot-byte sums.
    ///
    /// ```
    /// use kevy_alloc::{Heap, segment::{self, FOREIGN_SIZE_OFFSET}};
    /// let mut owner = Heap::new(1);
    /// let p = owner.alloc(100, 8).ok_or("no mapping")?;
    /// // freeing `p` from another shard: size into the slot, then a one-slot chain
    /// // SAFETY: `p` is a live slot of this class (104 bytes), ours to overwrite.
    /// unsafe { p.as_ptr().add(FOREIGN_SIZE_OFFSET).cast::<u32>().write(100) };
    /// // SAFETY: `p` is a live small slot, so it lies inside a segment.
    /// let seg = unsafe { segment::segment_of(p).as_ref() };
    /// // SAFETY: a one-slot chain of this segment's slot, with its sums.
    /// unsafe { seg.splice_foreign(p.as_ptr(), p.as_ptr(), 100, 104) };
    /// assert_eq!((seg.foreign_live(), seg.foreign_bytes()), (100, 104));
    /// owner.drain_foreign(); // the owner settles it
    /// assert_eq!(owner.snapshot().live, 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub unsafe fn splice_foreign(
        &self,
        head: *mut u8,
        tail: *mut u8,
        live_sum: usize,
        bytes_sum: usize,
    ) {
        // SAFETY: as in `foreign_bytes`.
        let home = unsafe { &*self.home };
        home.live.fetch_add(live_sum, Ordering::Relaxed);
        home.bytes.fetch_add(bytes_sum, Ordering::Relaxed);
        let mut old = self.foreign.load(Ordering::Relaxed);
        loop {
            // SAFETY: the tail is ours until the CAS below publishes the
            // chain; its link word is free to point at the current head.
            unsafe { tail.cast::<*mut u8>().write(old) };
            match self.foreign.compare_exchange_weak(old, head, Ordering::SeqCst, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => old = actual,
            }
        }
        // one push per queueing, however many splices land meanwhile
        // (see `ForeignTally::drain_pending` for the ordering)
        if !self.queued.load(Ordering::SeqCst) && !self.queued.swap(true, Ordering::SeqCst) {
            home.push_pending(self);
        }
    }

    /// Take the whole foreign-free list, leaving it empty. Only the owning
    /// shard may call this — that exclusivity is what makes the structure
    /// ABA-free (see [`Segment::splice_foreign`]).
    ///
    /// The taken bytes stay in [`Segment::foreign_bytes`] until the heap
    /// settles them by what it actually drained, which
    /// [`Heap::drain_foreign`](crate::Heap::drain_foreign) does.
    ///
    /// ```
    /// # use kevy_alloc::{Heap, segment};
    /// let mut heap = Heap::new(0);
    /// if let Some(p) = heap.alloc(64, 8) {
    ///     // SAFETY: `p` is a small slot this heap handed out.
    ///     let seg = unsafe { segment::segment_of(p).as_ref() };
    ///     assert!(seg.take_foreign().is_null(), "no other thread freed here");
    ///     // SAFETY: allocated just above with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// ```
    #[must_use]
    pub fn take_foreign(&self) -> *mut u8 {
        // an empty list is the common case on a drain; reading first
        // spares every idle segment's header a write and a swap
        if self.foreign.load(Ordering::Relaxed).is_null() {
            return core::ptr::null_mut();
        }
        self.foreign.swap(core::ptr::null_mut(), Ordering::Acquire)
    }
}
