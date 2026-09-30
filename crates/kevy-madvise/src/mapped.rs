//! The running total of what [`crate::mmap_anon_aligned_2mb`] holds mapped.

use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Bytes mapped and not yet unmapped, across every thread.
static MAPPED: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn note_mapped(bytes: usize) {
    MAPPED.fetch_add(bytes, Relaxed);
}

#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn note_unmapped(bytes: usize) {
    MAPPED.fetch_sub(bytes, Relaxed);
}

/// Bytes this process holds in mappings made by
/// [`mmap_anon_aligned_2mb`](crate::mmap_anon_aligned_2mb) and not yet
/// released with [`munmap_2mb`](crate::munmap_2mb), counted at their
/// mapped (2 MiB-rounded) length.
///
/// These mappings bypass the allocator, so an allocator's own statistics
/// never see them; a caller adding up what the process holds adds this.
/// 0 where the helper maps nothing (off Linux).
///
/// # Examples
///
/// ```
/// let before = kevy_madvise::mapped_bytes();
/// if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(3 << 20) {
///     // counted at the whole huge pages the mapping holds
///     assert_eq!(kevy_madvise::mapped_bytes() - before, 4 << 20);
///     // SAFETY: `p` is this call's own mapping, released exactly once.
///     unsafe { kevy_madvise::munmap_2mb(p, 3 << 20) };
/// }
/// assert_eq!(kevy_madvise::mapped_bytes(), before);
/// ```
#[must_use]
pub fn mapped_bytes() -> usize {
    MAPPED.load(Relaxed)
}

/// Hand the pages of `[ptr, ptr + len)` back to the kernel without
/// unmapping them, whole 2 MiB pages at a time: the range stays mapped
/// (and counted in [`mapped_bytes`]) and reads as zeros afterwards, but no
/// longer holds memory. `len` is rounded down to a 2 MiB multiple.
///
/// For a caller emptying one of these mappings in order before it unmaps
/// it — a table moving its entries to a bigger one — so the process does
/// not hold both whole at once. A no-op off Linux.
///
/// # Safety
///
/// `ptr` must be 2 MiB-aligned inside a live mapping from
/// [`mmap_anon_aligned_2mb`](crate::mmap_anon_aligned_2mb) that covers
/// `len` bytes, and nothing may read the range expecting its old contents.
///
/// # Examples
///
/// ```
/// let len = 4 << 20;
/// if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(len) {
///     // SAFETY: `p` is this call's mapping; the first 2 MiB are written,
///     // then released, then read back as the zeros the kernel maps in.
///     unsafe {
///         p.as_ptr().write_bytes(7, 2 << 20);
///         kevy_madvise::release_2mb(p, 2 << 20);
///         assert_eq!(p.as_ptr().read(), 0);
///         kevy_madvise::munmap_2mb(p, len);
///     }
/// }
/// ```
pub unsafe fn release_2mb(ptr: core::ptr::NonNull<u8>, len: usize) {
    if cfg!(miri) {
        let _ = (ptr, len);
        return;
    }
    #[cfg(target_os = "linux")]
    {
        const MADV_DONTNEED: core::ffi::c_int = 4;
        let whole = len & !(crate::HUGE_PAGE - 1);
        if whole > 0 {
            // SAFETY: the caller hands a 2 MiB-aligned range inside one of
            // this crate's anonymous private mappings and gives up its
            // contents; MADV_DONTNEED drops the pages and later reads fault
            // in zero pages, which is what the caller asked for.
            unsafe {
                let _ = crate::ffi::madvise(ptr.as_ptr().cast(), whole, MADV_DONTNEED);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ptr, len);
    }
}
