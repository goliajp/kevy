//! The `#[global_allocator]` shim.
//!
//! One [`Heap`] per thread, reached through thread-local storage. There
//! is no lock and no shared heap behind it: a thread allocates from its
//! own segments, and a free that arrives on the wrong thread is handed
//! back through the owning segment's push-only foreign list.
//!
//! # Two hazards this file exists to handle
//!
//! **Thread exit must not unmap live memory.** kevy shares values across
//! shards, so a segment can hold slots that outlive the thread that
//! allocated them. If the thread-local heap were dropped at thread exit
//! it would unmap those segments underneath their readers. The heap is
//! therefore held in a [`ManuallyDrop`], and its segments are
//! deliberately leaked when a thread ends — address space is given up,
//! never live memory. Handing abandoned segments to another heap the way
//! mimalloc does is the better answer and is not attempted here; leaking
//! is the answer that cannot be wrong.
//!
//! That also keeps the TLS block destructor-free, so access is a plain
//! static offset that cannot fail during teardown — a global allocator
//! that panics once TLS is gone is a bad way to end a process.
//!
//! **The allocator must not allocate.** Nothing on these paths uses
//! `Vec`, `Box` or formatting; segments are tracked through an intrusive
//! list threaded through their own headers, and the size-class table is
//! a `const` array.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::class;
use crate::heap::Heap;

thread_local! {
    /// `const` init keeps this a static offset with no lazy setup, and
    /// `ManuallyDrop` keeps the block free of destructors — see the
    /// module docs for why neither is incidental.
    static HEAP: UnsafeCell<ManuallyDrop<Heap>> =
        const { UnsafeCell::new(ManuallyDrop::new(Heap::new(0))) };
}

/// Run `f` against this thread's heap.
///
/// Returns `None` only when thread-local storage is unavailable, which
/// on a destructor-free block means the thread is past teardown. The
/// caller answers a null rather than panicking.
fn with_heap<R>(f: impl FnOnce(&mut Heap) -> R) -> Option<R> {
    HEAP.try_with(|cell| {
        // SAFETY: the cell is thread-local, so this is the only
        // reference in existence, and `f` cannot re-enter the allocator
        // (nothing on these paths allocates).
        let heap = unsafe { &mut *cell.get() };
        heap.ensure_identity();
        f(heap)
    })
    .ok()
}

/// A `#[global_allocator]` backed by one [`Heap`] per thread.
///
/// ```no_run
/// #[global_allocator]
/// static ALLOC: kevy_alloc::KevyAlloc = kevy_alloc::KevyAlloc;
/// ```
#[derive(Debug)]
pub struct KevyAlloc;

/// Bytes reserved before an over-aligned block to remember its base.
const BASE_SLOT: usize = core::mem::size_of::<usize>();

/// Total to request so that an `align`-aligned address with room for a
/// base pointer in front of it fits inside.
fn over_aligned_total(layout: Layout) -> Option<usize> {
    layout.size().checked_add(layout.align())?.checked_add(BASE_SLOT)
}

/// Whether a layout needs the over-aligned dance at all.
fn is_over_aligned(layout: Layout) -> bool {
    layout.align() > class::MAX_NATIVE_ALIGN
        && !(layout.size() > class::MAX_SMALL && layout.align() <= crate::os::PAGE)
}

// SAFETY: the four premises `GlobalAlloc` asks for, in order.
//
// 1. A returned block meets the layout. Alignments up to
//    `class::MAX_NATIVE_ALIGN` are what the size classes are built on;
//    anything stricter goes through `alloc_over_aligned`, which
//    over-allocates and rounds up, so the address it returns is aligned
//    by construction and has `layout.size()` bytes after it.
// 2. Failure is a null pointer, never an unwind. Every path out of
//    `with_heap` is an `Option`: `try_with` yields `None` once the
//    thread's `HEAP` is gone or not yet made, and the heap itself
//    returns `None` when it cannot serve, and both land on
//    `core::ptr::null_mut()`. Nothing here can panic on the failure
//    path, which is what makes it usable as THE allocator.
// 3. It is callable from any thread, and a block may cross threads. Each
//    thread has its own `Heap`, so there is no shared mutable state to
//    race on. A free arriving on a thread that did not allocate the
//    block is the case that would otherwise be unsound: `dealloc_small`
//    compares `seg.owner` against `self.id` and, when they differ,
//    pushes to the local outbound ring for the owner to drain rather
//    than touching the owner's segment or its non-atomic counters
//    (`heap_free.rs`). Ownership is read from the segment header, so it
//    is a property of the block, not of who is asking.
// 4. Re-entrancy cannot occur. The closure `with_heap` runs holds the
//    only reference to the thread's heap, and no path inside it
//    allocates — which is the premise the reference in `with_heap`
//    rests on in turn.
unsafe impl GlobalAlloc for KevyAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if is_over_aligned(layout) {
            return alloc_over_aligned(layout);
        }
        match with_heap(|h| h.alloc(layout.size(), layout.align())) {
            Some(Some(p)) => p.as_ptr(),
            _ => core::ptr::null_mut(),
        }
    }

    /// Grow or shrink without moving where the size class allows it.
    ///
    /// `GlobalAlloc`'s default implementation always allocates, copies
    /// and frees. That is what a pub/sub profile caught: the system
    /// allocator's `realloc` was visible and cheap, extending buffers
    /// where they lay, while ours copied every time a buffer grew. Since
    /// a class spans a range of sizes, most growth steps land back in
    /// the same class and need no work at all.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if let Some(p) = NonNull::new(ptr)
            && !is_over_aligned(layout)
            && new_size <= class::MAX_SMALL
            && with_heap(|h| {
                // SAFETY: `ptr` is live and was made with this layout.
                unsafe { h.try_resize_in_place(p, layout.size(), new_size, layout.align()) }
            }) == Some(true)
        {
            return ptr;
        }
        // SAFETY: the default dance — allocate, copy the smaller of the
        // two lengths, release the old block.
        unsafe {
            let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
                return core::ptr::null_mut();
            };
            let fresh = self.alloc(new_layout);
            if !fresh.is_null() {
                core::ptr::copy_nonoverlapping(ptr, fresh, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            fresh
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(p) = NonNull::new(ptr) else { return };
        if is_over_aligned(layout) {
            // SAFETY: produced by `alloc_over_aligned` with this layout.
            unsafe { dealloc_over_aligned(p, layout) };
            return;
        }
        with_heap(|h| {
            // SAFETY: delegated to `GlobalAlloc`'s contract — same
            // layout the allocation was made with.
            unsafe { h.dealloc(p, layout.size(), layout.align()) };
        });
    }
}

/// Serve an alignment stricter than a size class can offer by
/// over-allocating and recording the base pointer just below the
/// aligned address.
///
/// The base has to be recorded because the aligned address is not
/// derivable from the layout alone: it depends on where the underlying
/// block landed. This is the one place the crate stores a header, and it
/// is confined to a path Rust programs take rarely.
fn alloc_over_aligned(layout: Layout) -> *mut u8 {
    let Some(total) = over_aligned_total(layout) else {
        return core::ptr::null_mut();
    };
    let Some(Some(base)) = with_heap(|h| h.alloc(total, class::MIN_ALIGN)) else {
        return core::ptr::null_mut();
    };
    let raw = base.as_ptr() as usize;
    let aligned = (raw + BASE_SLOT + layout.align() - 1) & !(layout.align() - 1);
    // SAFETY: `aligned - BASE_SLOT >= raw` by construction, and the
    // block is ours and at least `total` bytes.
    unsafe { ((aligned - BASE_SLOT) as *mut usize).write(raw) };
    aligned as *mut u8
}

/// # Safety
/// `ptr` must come from [`alloc_over_aligned`] with the same layout.
unsafe fn dealloc_over_aligned(ptr: NonNull<u8>, layout: Layout) {
    let Some(total) = over_aligned_total(layout) else {
        return;
    };
    // SAFETY: the base pointer sits immediately below the aligned
    // address, written when the block was handed out.
    let raw = unsafe { ((ptr.as_ptr() as usize - BASE_SLOT) as *const usize).read() };
    let Some(base) = NonNull::new(raw as *mut u8) else {
        return;
    };
    with_heap(|h| {
        // SAFETY: the block was allocated with exactly this size and
        // alignment on this thread's heap or another's — `Heap::dealloc`
        // routes foreign frees home itself.
        unsafe { h.dealloc(base, total, class::MIN_ALIGN) };
    });
}

/// This thread's heap statistics, or `None` past thread teardown.
///
/// Shards report separately; a process figure is [`crate::Stats::merge`]
/// over them.
/// # Examples
///
/// ```
/// // `None` past thread teardown — a caller cannot read that as
/// // "this thread allocated nothing".
/// if let Some(s) = kevy_alloc::thread_stats() {
///     // Every mapped byte lands in exactly one bucket, so the sum is
///     // comparable to `mapped` rather than derived from it.
///     assert!(s.accounted() <= s.mapped);
/// }
/// ```
#[must_use]
pub fn thread_stats() -> Option<crate::Stats> {
    with_heap(|h| h.snapshot())
}

/// Return this thread's empty spans to the OS.
///
/// Exposed rather than run automatically because how often to sweep is a
/// policy question the engine answers, not the allocator: kevy already
/// has a shard tick to hang it on.
/// # Examples
///
/// ```
/// // Idempotent and always safe to call: with nothing to return it
/// // does nothing, which is why the engine can hang it on a tick.
/// kevy_alloc::thread_reclaim();
/// kevy_alloc::thread_reclaim();
/// ```
pub fn thread_reclaim() {
    with_heap(Heap::reclaim);
}
