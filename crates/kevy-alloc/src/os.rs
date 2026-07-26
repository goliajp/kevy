//! The OS boundary: anonymous mapping, unmapping, and returning pages.
//!
//! Three hand-declared `extern "C"` symbols, no `libc` crate — the house
//! rule for OS boundaries. Linux and macOS only; elsewhere every entry
//! point reports failure and the allocator is simply unavailable.
//!
//! # Why not `kevy-madvise`
//!
//! That crate already binds `mmap`/`munmap`/`madvise`, so reusing it was
//! the first choice. It does not fit: it is Linux-only by construction
//! and its contract *is* huge-page advice — every mapping it hands out
//! has `MADV_HUGEPAGE` applied. An allocator needs mappings on macOS too
//! (that is where this is developed), and it must be able to *return*
//! pages, which is the property the whole experiment rests on. Widening
//! a crate whose name is its contract costs more than three extern
//! declarations, so the boundary lives here — which is also why
//! `kevy-alloc` is in the recorded unsafe set (allocgate M8).

#[cfg(any(target_os = "linux", target_os = "macos"))]
use core::ffi::c_void;
use core::ptr::NonNull;

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> i32;
    fn madvise(addr: *mut c_void, length: usize, advice: i32) -> i32;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROT_READ: i32 = 0x1;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROT_WRITE: i32 = 0x2;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAP_PRIVATE: i32 = 0x2;

#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: i32 = 0x1000;

/// Discard the contents of a resident range and return the physical
/// pages to the OS, keeping the mapping addressable.
///
/// Linux `MADV_DONTNEED` (4) drops the pages outright: RSS falls and a
/// later touch faults in a zero page. macOS has no equivalent that
/// *guarantees* the drop — `MADV_FREE` (5) marks pages reclaimable and
/// the kernel takes them under pressure, so RSS may not move promptly.
/// The difference is why M4 (reclaim proven directly) is asserted on
/// Linux and reported as informational on macOS rather than being
/// quietly assumed to hold on both.
#[cfg(target_os = "linux")]
const MADV_DISCARD: i32 = 4;
#[cfg(target_os = "macos")]
const MADV_DISCARD: i32 = 5;

/// The system page size this module assumes for rounding.
pub const PAGE: usize = 4096;

/// Round `n` up to a multiple of `align`, which must be a power of two.
#[must_use]
pub const fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// Map `len` bytes anonymously with the returned address aligned to
/// `align` bytes.
///
/// `align` must be a power of two and a multiple of [`PAGE`]; `len` must
/// be a non-zero multiple of `align`. Over-allocates by one alignment
/// unit and trims both sides, because `mmap` only promises page
/// alignment. Returns `None` on failure — never panics, because an
/// allocator that panics on OOM is worse than one that reports it.
pub fn map_aligned(len: usize, align: usize) -> Option<NonNull<u8>> {
    if len == 0 || !align.is_power_of_two() || !len.is_multiple_of(align) {
        return None;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if cfg!(miri) {
            return None;
        }
        let total = len.checked_add(align)?;
        // SAFETY: the canonical anonymous mapping call. No Rust memory is
        // read or written; a null hint lets the kernel choose the address.
        let raw = unsafe {
            mmap(
                core::ptr::null_mut(),
                total,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw as isize == -1 {
            return None;
        }
        NonNull::new(trim(raw as usize, total, len, align) as *mut u8)
    }
}

/// Trim an over-allocated mapping down to `len` bytes starting at the
/// first `align`-aligned address inside it, unmapping both offcuts.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn trim(raw: usize, total: usize, len: usize, align: usize) -> usize {
    let start = (raw + align - 1) & !(align - 1);
    let prefix = start - raw;
    let suffix = total - prefix - len;
    if prefix > 0 {
        // SAFETY: `prefix` bytes at `raw` are part of the mapping we
        // just made and are not otherwise referenced.
        unsafe { munmap(raw as *mut c_void, prefix) };
    }
    if suffix > 0 {
        // SAFETY: same mapping, the tail past the aligned region.
        unsafe { munmap((start + len) as *mut c_void, suffix) };
    }
    start
}

/// Unmap `len` bytes at `ptr`.
///
/// # Safety
/// `ptr`/`len` must describe a live mapping produced by [`map_aligned`]
/// (or a whole sub-range of one that is no longer referenced).
pub unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if cfg!(miri) {
            return;
        }
        // SAFETY: delegated to the caller's contract.
        unsafe { munmap(ptr.as_ptr().cast(), len) };
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (ptr, len);
    }
}

/// Return the physical pages backing `len` bytes at `ptr` to the OS
/// while keeping the range mapped and addressable.
///
/// The range must be page-aligned and a whole number of pages. Contents
/// are discarded: a later read sees zeroes, which is why only spans with
/// no live slots are ever passed here.
///
/// # Safety
/// `ptr`/`len` must lie inside a live mapping from [`map_aligned`], and
/// no live data may remain in the range.
pub unsafe fn discard(ptr: NonNull<u8>, len: usize) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if cfg!(miri) {
            return false;
        }
        // SAFETY: delegated to the caller's contract; madvise reads no
        // Rust memory.
        unsafe { madvise(ptr.as_ptr().cast(), len, MADV_DISCARD) == 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (ptr, len);
        false
    }
}

/// Whether this target can map memory at all. Used by tests and by the
/// heap's construction path to fail fast rather than mysteriously.
#[must_use]
pub const fn available() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos")) && !cfg!(miri)
}
