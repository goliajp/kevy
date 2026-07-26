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
#[must_use]
pub fn thread_stats() -> Option<crate::Stats> {
    with_heap(|h| h.snapshot())
}

/// Return this thread's empty spans to the OS.
///
/// Exposed rather than run automatically because how often to sweep is a
/// policy question the engine answers, not the allocator: kevy already
/// has a shard tick to hang it on.
pub fn thread_reclaim() {
    with_heap(Heap::reclaim);
}
