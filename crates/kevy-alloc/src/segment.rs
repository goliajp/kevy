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
//!
//! # Examples
//!
//! ```
//! use kevy_alloc::{Heap, segment};
//! let mut heap = Heap::new(0);
//! let p = heap.alloc(100, 8).ok_or("no mapping")?;
//! // everything `dealloc` needs comes from the address: segment, span, class
//! // SAFETY: `p` is a live small slot, so it lies inside a segment.
//! let seg = unsafe { segment::segment_of(p).as_ref() };
//! let span = &seg.spans()[segment::span_index_of(p)];
//! assert_eq!(kevy_alloc::class::size_of(usize::from(span.class())), 104);
//! // SAFETY: `p` came from this heap with this size and alignment.
//! unsafe { heap.dealloc(p, 100, 8) };
//! # Ok::<(), &str>(())
//! ```

use core::ptr::NonNull;
use core::sync::atomic::AtomicPtr;

use crate::class::{self, SPAN_BYTES};
pub use crate::pagemap::{NO_CLASS, SpanMeta};
pub(crate) use segment_foreign::ForeignTally;

/// Bytes per segment. Power of two: the mask is the lookup.
///
/// ```
/// use kevy_alloc::{Heap, segment::SEGMENT_BYTES};
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(64, 8).ok_or("no mapping")?;
/// // masking off the low bits finds the segment base
/// let base = p.as_ptr() as usize & !(SEGMENT_BYTES - 1);
/// assert_eq!(base % SEGMENT_BYTES, 0);
/// assert!(p.as_ptr() as usize - base < SEGMENT_BYTES);
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
pub const SEGMENT_BYTES: usize = 4 * 1024 * 1024;

/// Spans in a segment, including the header span.
///
/// ```
/// use kevy_alloc::{class::SPAN_BYTES, segment::{SEGMENT_BYTES, SPANS_PER_SEGMENT}};
/// assert_eq!(SPANS_PER_SEGMENT, 64);
/// assert_eq!(SPANS_PER_SEGMENT * SPAN_BYTES, SEGMENT_BYTES);
/// ```
pub const SPANS_PER_SEGMENT: usize = SEGMENT_BYTES / SPAN_BYTES;

/// Span index 0 is the header; allocation spans start at 1.
///
/// ```
/// use kevy_alloc::{Heap, segment::{FIRST_DATA_SPAN, span_index_of}};
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(64, 8).ok_or("no mapping")?;
/// // span 0 is the header, so no slot is ever handed out there
/// assert!(span_index_of(p) >= FIRST_DATA_SPAN);
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
pub const FIRST_DATA_SPAN: usize = 1;

/// Identifies a live segment header. A pointer that masks to something
/// without this word is not ours, and that is a bug in the caller
/// rather than something to paper over.
const MAGIC: u64 = 0x6b65_7679_616c_6c63; // "kevyallc"

/// The header at the base of every segment.
///
/// ```
/// use kevy_alloc::{Heap, segment::{Segment, segment_of}};
/// let mut heap = Heap::new(3);
/// let p = heap.alloc(64, 8).ok_or("no mapping")?;
/// // SAFETY: `p` is a live small slot, so it lies inside a segment.
/// let seg: &Segment = unsafe { segment_of(p).as_ref() };
/// assert!(seg.is_valid());
/// assert_eq!(seg.owner(), 3);
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
#[repr(C)]
#[derive(Debug)]
pub struct Segment {
    magic: u64,
    /// Intrusive list of a heap's segments — an allocator cannot use a
    /// `Vec` to track its own memory without recursing into itself.
    pub(crate) next: *mut Segment,
    /// The shard that owns every span here. Foreign frees find their
    /// way home through this.
    pub(crate) owner: usize,
    /// Slots freed by a thread other than the owner, as a lock-free
    /// stack of slot addresses. See [`Segment::splice_foreign`] for why this is
    /// push-only.
    pub(crate) foreign: AtomicPtr<u8>,
    /// Slot bytes parked on the foreign lists of every segment of this
    /// heap. Only the heap's first segment's copy is used; the others
    /// point at it through `home`, so the owner prices everything
    /// parked with one read instead of a walk of its segments.
    pub(crate) parked: ForeignTally,
    /// The `parked` this segment's splices post to.
    pub(crate) home: *const ForeignTally,
    /// Per-span bookkeeping, indexed by span number. Index 0 describes
    /// the header span itself and is never assigned a class.
    pub(crate) spans: [SpanMeta; SPANS_PER_SEGMENT],
    /// Each span's place on its heap's lists (see `spanlist`), beside the
    /// metadata rather than in it so the metadata stays plain data.
    pub(crate) links: [crate::spanlist::SpanLink; SPANS_PER_SEGMENT],
}

impl Segment {
    /// Initialise a freshly mapped segment in place.
    ///
    /// # Safety
    /// `base` must be a live, writable, 4 MiB-aligned mapping of
    /// [`SEGMENT_BYTES`] bytes that nothing else references.
    ///
    /// ```
    /// use kevy_alloc::os::{map_aligned, unmap};
    /// use kevy_alloc::segment::{SEGMENT_BYTES, Segment};
    /// let base = map_aligned(SEGMENT_BYTES, SEGMENT_BYTES).ok_or("no mapping")?;
    /// // SAFETY: a fresh, writable, aligned mapping nothing else references.
    /// let seg = unsafe { Segment::init(base, 9) };
    /// // SAFETY: `init` just wrote a header there.
    /// let hdr = unsafe { seg.as_ref() };
    /// assert!(hdr.is_valid());
    /// assert_eq!(hdr.owner(), 9);
    /// // SAFETY: the whole mapping, no longer referenced.
    /// unsafe { unmap(base, SEGMENT_BYTES) };
    /// # Ok::<(), &str>(())
    /// ```
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
                parked: ForeignTally::new(),
                home: core::ptr::null(),
                spans: [SpanMeta::new(); SPANS_PER_SEGMENT],
                links: [crate::spanlist::SpanLink::NONE; SPANS_PER_SEGMENT],
            });
            (*seg).home = &raw const (*seg).parked;
        }
        // SAFETY: just written.
        unsafe { NonNull::new_unchecked(seg) }
    }

    /// Base address of span `index` within this segment.
    ///
    /// ```
    /// use kevy_alloc::{Heap, class::SPAN_BYTES, segment};
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// // SAFETY: `p` is a live small slot, so it lies inside a segment.
    /// let seg = unsafe { segment::segment_of(p).as_ref() };
    /// let base = seg.span_base(segment::span_index_of(p)) as usize;
    /// assert!(base <= p.as_ptr() as usize && (p.as_ptr() as usize) < base + SPAN_BYTES);
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 64, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn span_base(&self, index: usize) -> *mut u8 {
        let base = core::ptr::from_ref(self) as usize;
        (base + index * SPAN_BYTES) as *mut u8
    }

    /// Check the header is one of ours. A false result means a pointer
    /// reached `dealloc` that this allocator never handed out.
    ///
    /// ```
    /// use kevy_alloc::{Heap, segment};
    /// let mut heap = Heap::new(0);
    /// let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// // SAFETY: `p` is a live small slot, so it lies inside a segment.
    /// assert!(unsafe { segment::segment_of(p).as_ref() }.is_valid());
    /// // SAFETY: `p` came from this heap with this size and alignment.
    /// unsafe { heap.dealloc(p, 64, 8) };
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.magic == MAGIC
    }

    /// The shard that owns every span here.
    ///
    /// ```
    /// # use kevy_alloc::{Heap, segment};
    /// let mut heap = Heap::new(7);
    /// if let Some(p) = heap.alloc(64, 8) {
    ///     // SAFETY: `p` is a small slot this heap handed out.
    ///     let seg = unsafe { segment::segment_of(p).as_ref() };
    ///     assert_eq!(seg.owner(), 7);
    ///     // SAFETY: allocated just above with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// ```
    #[must_use]
    pub fn owner(&self) -> usize {
        self.owner
    }

    /// Per-span bookkeeping, indexed by span number. Index 0 describes
    /// the header span itself and is never assigned a class.
    ///
    /// ```
    /// # use kevy_alloc::{Heap, segment};
    /// let mut heap = Heap::new(0);
    /// if let Some(p) = heap.alloc(64, 8) {
    ///     // SAFETY: `p` is a small slot this heap handed out.
    ///     let seg = unsafe { segment::segment_of(p).as_ref() };
    ///     assert_eq!(seg.spans()[0].class(), segment::NO_CLASS);
    ///     // SAFETY: allocated just above with this size and alignment.
    ///     unsafe { heap.dealloc(p, 64, 8) };
    /// }
    /// ```
    #[must_use]
    pub fn spans(&self) -> &[SpanMeta; SPANS_PER_SEGMENT] {
        &self.spans
    }
}

/// Recover the segment owning `ptr`.
///
/// # Safety
/// `ptr` must be a slot address previously handed out by a segment of
/// this allocator (that is, not from the direct-mapping path).
///
/// ```
/// use kevy_alloc::{Heap, segment::{SEGMENT_BYTES, segment_of}};
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(64, 8).ok_or("no mapping")?;
/// // SAFETY: `p` came from a segment, not from the direct-mapping path.
/// let seg = unsafe { segment_of(p) };
/// assert_eq!(seg.as_ptr() as usize, p.as_ptr() as usize & !(SEGMENT_BYTES - 1));
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
#[inline]
#[must_use]
pub unsafe fn segment_of(ptr: NonNull<u8>) -> NonNull<Segment> {
    let base = ptr.as_ptr() as usize & !(SEGMENT_BYTES - 1);
    // SAFETY: by the caller's contract this address is a live segment
    // header, since every slot lies inside one.
    unsafe { NonNull::new_unchecked(base as *mut Segment) }
}

/// The span index within a segment holding `ptr`.
///
/// ```
/// use kevy_alloc::{Heap, segment::{FIRST_DATA_SPAN, SPANS_PER_SEGMENT, span_index_of}};
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(64, 8).ok_or("no mapping")?;
/// assert!((FIRST_DATA_SPAN..SPANS_PER_SEGMENT).contains(&span_index_of(p)));
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
#[inline]
#[must_use]
pub fn span_index_of(ptr: NonNull<u8>) -> usize {
    (ptr.as_ptr() as usize & (SEGMENT_BYTES - 1)) / SPAN_BYTES
}

/// The slot index within its span holding `ptr`, for a given class.
///
/// ```
/// use kevy_alloc::{Heap, class::index_of, segment::slot_index_of};
/// let mut heap = Heap::new(0);
/// let c = index_of(64, 8).ok_or("class")?;
/// let a = heap.alloc(64, 8).ok_or("no mapping")?;
/// let b = heap.alloc(64, 8).ok_or("no mapping")?;
/// // a fresh span hands out neighbouring slots, lowest first
/// assert_eq!(slot_index_of(b, c), slot_index_of(a, c) + 1);
/// // SAFETY: both came from this heap with this size and alignment.
/// unsafe { heap.dealloc(a, 64, 8); heap.dealloc(b, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
#[inline]
#[must_use]
pub fn slot_index_of(ptr: NonNull<u8>, class: usize) -> u32 {
    let off = ptr.as_ptr() as usize & (SPAN_BYTES - 1);
    // A multiply-shift, not a division: this runs on every free (twice
    // on the claims path), and the owner-thread srcline profile put the
    // `div` at the top of the post-reorder residue (3.9 %).
    class::slot_of_offset(off, class)
}

/// Where [`Segment::splice_foreign`] stores the requested size inside a free slot,
/// clear of the link that occupies the first word.
///
/// ```
/// use kevy_alloc::{class::CLASSES, segment::FOREIGN_SIZE_OFFSET};
/// // the link word comes first; the size (a u32) must fit in every class
/// assert_eq!(FOREIGN_SIZE_OFFSET, core::mem::size_of::<*mut u8>());
/// assert!(FOREIGN_SIZE_OFFSET + 4 <= CLASSES[0] as usize);
/// ```
pub const FOREIGN_SIZE_OFFSET: usize = core::mem::size_of::<*mut u8>();

/// Read back the requested size a foreign free recorded.
///
/// # Safety
/// `slot` must still be on a foreign list, untouched since
/// [`Segment::splice_foreign`] wrote it.
///
/// ```
/// use kevy_alloc::{Heap, segment::{self, FOREIGN_SIZE_OFFSET, foreign_requested}};
/// let mut owner = Heap::new(1);
/// let p = owner.alloc(100, 8).ok_or("no mapping")?;
/// // SAFETY: `p` is a live 104-byte slot, ours to overwrite.
/// unsafe { p.as_ptr().add(FOREIGN_SIZE_OFFSET).cast::<u32>().write(100) };
/// // SAFETY: `p` is a live small slot, so it lies inside a segment.
/// let seg = unsafe { segment::segment_of(p).as_ref() };
/// // SAFETY: a one-slot chain of this segment's slot, with its sums.
/// unsafe { seg.splice_foreign(p.as_ptr(), p.as_ptr(), 100, 104) };
/// // SAFETY: `p` is queued and untouched since the splice.
/// assert_eq!(unsafe { foreign_requested(p) }, 100);
/// owner.drain_foreign();
/// # Ok::<(), &str>(())
/// ```
#[must_use]
pub unsafe fn foreign_requested(slot: NonNull<u8>) -> usize {
    // SAFETY: written by `Segment::splice_foreign`, and nothing hands out a slot
    // while it is queued.
    unsafe { slot.as_ptr().add(FOREIGN_SIZE_OFFSET).cast::<u32>().read() as usize }
}

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;

#[path = "segment_foreign.rs"]
mod segment_foreign;
