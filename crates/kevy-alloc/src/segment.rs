//! Segments and spans — where a pointer's identity comes from.
//!
//! A **segment** is a 4 MiB region mapped at a 4 MiB-aligned address. It
//! is cut into 64 **spans** of 64 KiB; span 0 holds the segment header
//! and the other 63 serve allocations, one size class each.
//!
//! That geometry is the whole reason there are no per-allocation
//! headers. Masking a pointer with `!(SEGMENT_BYTES - 1)` gives the
//! segment, the offset gives the span index, and the span's metadata
//! gives the class — so `dealloc` recovers everything it needs from the
//! address itself. glibc has to store a size beside every chunk because
//! C's `free` is not told one; we are, and even when we were not, the
//! address would answer.
//!
//! Reference: mimalloc's segment/page split (`segment.c`), and Go's
//! `mheap` arena indexing. The divergence from both is that a segment
//! here is owned by exactly one shard for its whole life — kevy pins a
//! shard per core, so ownership never has to be negotiated.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::class::{self, SPAN_BYTES};
pub use crate::pagemap::{NO_CLASS, SpanMeta};

/// Bytes per segment. Power of two: the mask is the lookup.
pub const SEGMENT_BYTES: usize = 4 * 1024 * 1024;

/// Spans in a segment, including the header span.
pub const SPANS_PER_SEGMENT: usize = SEGMENT_BYTES / SPAN_BYTES;

/// Span index 0 is the header; allocation spans start at 1.
pub const FIRST_DATA_SPAN: usize = 1;

/// Identifies a live segment header. A pointer that masks to something
/// without this word is not ours, and that is a bug in the caller
/// rather than something to paper over.
const MAGIC: u64 = 0x6b65_7679_616c_6c63; // "kevyallc"

/// The header at the base of every segment.
#[repr(C)]
pub struct Segment {
    magic: u64,
    /// Intrusive list of a heap's segments — an allocator cannot use a
    /// `Vec` to track its own memory without recursing into itself.
    pub next: *mut Segment,
    /// The shard that owns every span here. Foreign frees find their
    /// way home through this.
    pub owner: usize,
    /// Slots freed by a thread other than the owner, as a lock-free
    /// stack of slot addresses. See [`push_foreign`] for why this is
    /// push-only.
    pub foreign: AtomicPtr<u8>,
    /// Slot bytes parked on `foreign`, so the accounting can price the
    /// list without walking it. Bytes rather than a count: one list
    /// carries slots of several classes, so a count cannot be converted
    /// back.
    ///
    /// `AtomicUsize` rather than `AtomicU64` because 32-bit targets
    /// (Cortex-M among them) have no 64-bit atomic, and a pending
    /// foreign-free list cannot exceed the address space anyway.
    pub foreign_bytes: core::sync::atomic::AtomicUsize,
    /// Of those, the bytes callers actually asked for.
    ///
    /// The owner's `live`/`rounding` counters still include everything on
    /// this list, because the thread that freed it cannot touch another
    /// thread's counters. Snapshots move the amount across so it is
    /// counted once — see `Heap::snapshot`.
    pub foreign_live: core::sync::atomic::AtomicUsize,
    /// Per-span bookkeeping, indexed by span number. Index 0 describes
    /// the header span itself and is never assigned a class.
    pub spans: [SpanMeta; SPANS_PER_SEGMENT],
}

impl Segment {
    /// Initialise a freshly mapped segment in place.
    ///
    /// # Safety
    /// `base` must be a live, writable, 4 MiB-aligned mapping of
    /// [`SEGMENT_BYTES`] bytes that nothing else references.
    pub unsafe fn init(base: NonNull<u8>, owner: usize) -> NonNull<Segment> {
        let seg = base.as_ptr().cast::<Segment>();
        // SAFETY: the caller guarantees an exclusive writable mapping
        // large enough for the header, which lives in span 0.
        unsafe {
            seg.write(Segment {
                magic: MAGIC,
                next: core::ptr::null_mut(),
                owner,
                foreign: AtomicPtr::new(core::ptr::null_mut()),
                foreign_bytes: core::sync::atomic::AtomicUsize::new(0),
                foreign_live: core::sync::atomic::AtomicUsize::new(0),
                spans: [SpanMeta::new(); SPANS_PER_SEGMENT],
            });
        }
        // SAFETY: just written.
        unsafe { NonNull::new_unchecked(seg) }
    }

    /// Base address of span `index` within this segment.
    #[must_use]
    pub fn span_base(&self, index: usize) -> *mut u8 {
        let base = core::ptr::from_ref(self) as usize;
        (base + index * SPAN_BYTES) as *mut u8
    }

    /// Check the header is one of ours. A false result means a pointer
    /// reached `dealloc` that this allocator never handed out.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.magic == MAGIC
    }
}

/// Recover the segment owning `ptr`.
///
/// # Safety
/// `ptr` must be a slot address previously handed out by a segment of
/// this allocator (that is, not from the direct-mapping path).
#[inline]
#[must_use]
pub unsafe fn segment_of(ptr: NonNull<u8>) -> NonNull<Segment> {
    let base = ptr.as_ptr() as usize & !(SEGMENT_BYTES - 1);
    // SAFETY: by the caller's contract this address is a live segment
    // header, since every slot lies inside one.
    unsafe { NonNull::new_unchecked(base as *mut Segment) }
}

/// The span index within a segment holding `ptr`.
#[inline]
#[must_use]
pub fn span_index_of(ptr: NonNull<u8>) -> usize {
    (ptr.as_ptr() as usize & (SEGMENT_BYTES - 1)) / SPAN_BYTES
}

/// The slot index within its span holding `ptr`, for a given class.
#[inline]
#[must_use]
pub fn slot_index_of(ptr: NonNull<u8>, class: usize) -> u32 {
    let off = ptr.as_ptr() as usize & (SPAN_BYTES - 1);
    (off / class::size_of(class)) as u32
}

/// Push a slot onto a segment's foreign-free stack.
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
/// `swap`.** There is no compare-and-swap on the consumer side, so
/// there is no window for ABA to open. Producers only ever push. This
/// is mimalloc's thread-free design, and it is strictly simpler than
/// tagged pointers or hazard pointers would have been.
///
/// The slot carries the requested size as well as the link, because the
/// owner needs it at drain time and nothing else remembers it: the
/// freeing thread knows it from the `Layout`, the owner does not, and
/// there is no per-allocation header to consult. A free slot's own bytes
/// are the natural place to put it — the smallest class is 16 bytes and
/// this needs a pointer plus four.
///
/// # Safety
/// `slot` must be a live slot address belonging to `seg`, no longer
/// referenced by anyone; `slot_size` must be its class's slot size and
/// `requested` the size it was allocated with.
pub unsafe fn push_foreign(
    seg: &Segment,
    slot: NonNull<u8>,
    slot_size: usize,
    requested: usize,
) {
    // SAFETY: the slot is ours and unreferenced, and every class is at
    // least 16 bytes — room for the link and the size beside it.
    unsafe {
        slot.as_ptr()
            .add(FOREIGN_SIZE_OFFSET)
            .cast::<u32>()
            .write(requested as u32);
    }
    seg.foreign_live.fetch_add(requested, Ordering::Relaxed);
    let mut head = seg.foreign.load(Ordering::Relaxed);
    loop {
        // SAFETY: same slot, still unreferenced; the first bytes hold
        // the link.
        unsafe { slot.as_ptr().cast::<*mut u8>().write(head) };
        match seg.foreign.compare_exchange_weak(
            head,
            slot.as_ptr(),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => head = actual,
        }
    }
    seg.foreign_bytes.fetch_add(slot_size, Ordering::Relaxed);
}

/// Take the whole foreign-free list, leaving it empty. Only the owning
/// shard may call this — that exclusivity is what makes the structure
/// ABA-free (see [`push_foreign`]).
#[must_use]
pub fn take_foreign(seg: &Segment) -> *mut u8 {
    seg.foreign_bytes.store(0, Ordering::Relaxed);
    seg.foreign_live.store(0, Ordering::Relaxed);
    seg.foreign.swap(core::ptr::null_mut(), Ordering::Acquire)
}

/// Where [`push_foreign`] stores the requested size inside a free slot,
/// clear of the link that occupies the first word.
pub const FOREIGN_SIZE_OFFSET: usize = core::mem::size_of::<*mut u8>();

/// Read back the requested size a foreign free recorded.
///
/// # Safety
/// `slot` must still be on a foreign list, untouched since
/// [`push_foreign`] wrote it.
#[must_use]
pub unsafe fn foreign_requested(slot: NonNull<u8>) -> usize {
    // SAFETY: written by `push_foreign`, and nothing hands out a slot
    // while it is queued.
    unsafe { slot.as_ptr().add(FOREIGN_SIZE_OFFSET).cast::<u32>().read() as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_fits_inside_the_span_it_occupies() {
        assert!(
            core::mem::size_of::<Segment>() <= SPAN_BYTES,
            "the header spills out of span 0 into an allocation span"
        );
    }

    #[test]
    fn geometry_is_maskable() {
        assert!(SEGMENT_BYTES.is_power_of_two());
        assert!(SPAN_BYTES.is_power_of_two());
        assert_eq!(SEGMENT_BYTES % SPAN_BYTES, 0);
        assert_eq!(SPANS_PER_SEGMENT, 64);
    }

    #[test]
    fn the_bitmap_header_still_fits_its_span() {
        // v2 made SpanMeta deliberately large — the bitmap is the price
        // of page-granular reclaim, and the header span exists to be
        // spent on exactly this. The bound that matters is the span.
        assert!(core::mem::size_of::<SpanMeta>() >= crate::pagemap::BITMAP_WORDS * 8);
        assert!(core::mem::size_of::<Segment>() <= SPAN_BYTES);
    }
}
