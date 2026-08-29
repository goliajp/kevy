//! kevy-madvise — thin pure-Rust `madvise` hints.
//!
//! A single best-effort kernel hint: tell Linux a region is a candidate for
//! transparent huge pages (`MADV_HUGEPAGE`). Hand-bound with `unsafe extern
//! "C"` against glibc — no `libc` crate, no third-party dependency. Off Linux
//! every entry point compile-time no-ops.
//!
//! Carved out of `kevy-sys` so it can be used by other library crates (like
//! `kevy-map`) without dragging the rest of the OS-boundary internals along. See
//! [`advise_hugepage`] for the only entry point.
//!
//! # Safety
//!
//! `unsafe` is confined to a single `extern "C"` declaration of `madvise(3)`
//! and one wrapper call site. The wrapper rounds the request to page
//! boundaries, never reads or writes Rust memory, and silently no-ops when
//! the kernel returns `EINVAL` — making it safe to expose as a plain `fn`.
//!
//! ```
//! // Every entry point is best-effort and reports nothing, so the same
//! // call compiles and runs on a platform that cannot honour it.
//! let buf = vec![0u8; 1 << 20];
//! kevy_madvise::advise_hugepage(buf.as_ptr(), buf.len());
//!
//! // The mapping helper says no rather than panicking, and its unmap
//! // takes the same length back.
//! if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(2 << 20) {
//!     unsafe { kevy_madvise::munmap_2mb(p, 2 << 20) };
//! }
//! ```

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(target_os = "linux")]
mod ffi {
    use core::ffi::{c_int, c_void};

    // The four libc symbols kevy-madvise touches; every call site is in this
    // file. glibc resolves these via `std`'s existing linkage — no extra
    // link directive needed.
    unsafe extern "C" {
        pub fn madvise(addr: *mut c_void, length: usize, advice: c_int) -> c_int;
        pub fn mmap(
            addr: *mut c_void,
            length: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, length: usize) -> c_int;
    }
}

/// Hint the kernel that the region `[ptr, ptr+len)` is a candidate for
/// transparent huge pages (Linux `MADV_HUGEPAGE`). A best-effort kernel
/// hint — returns nothing; mis-alignment / unsupported kernels silently
/// no-op. Off Linux this is a compile-time no-op.
///
/// Used by [`kevy-map`](https://crates.io/crates/kevy-map) to drop dTLB-load
/// misses on the metadata + slot arrays of large keyspace tables. madvise
/// expects page-aligned `addr` and a page-multiple `length`; we round addr
/// UP and len DOWN to 4 KiB. If nothing remains, we don't call. Regions
/// smaller than ~ a few pages are not worth a syscall.
/// # Examples
///
/// A hint, never a contract: it takes any region and reports nothing, so
/// the caller keeps the same code on every platform and every kernel.
/// Off Linux it compiles to nothing at all.
///
/// ```
/// let buf = vec![0u8; 4 * 1024 * 1024];
/// kevy_madvise::advise_hugepage(buf.as_ptr(), buf.len());
/// assert_eq!(buf.len(), 4 * 1024 * 1024, "the region is untouched by the hint");
/// ```
///
/// A region too small to be worth a syscall is dropped without one, and a
/// zero-length region is not passed to the kernel either. Neither is an
/// error to the caller.
///
/// ```
/// let tiny = [0u8; 8];
/// kevy_madvise::advise_hugepage(tiny.as_ptr(), tiny.len());
/// kevy_madvise::advise_hugepage(tiny.as_ptr(), 0);
/// ```
/// # Examples
///
/// ```
/// // Best-effort: a hint the kernel is free to ignore, and a no-op off
/// // Linux. It reports nothing, so a caller cannot branch on it and be
/// // wrong on another platform.
/// let buf = vec![0u8; 4096];
/// kevy_madvise::advise_hugepage(buf.as_ptr(), buf.len());
/// ```
pub fn advise_hugepage(ptr: *const u8, len: usize) {
    // Miri cannot execute foreign syscalls; madvise is purely advisory, so
    // a no-op under miri preserves correctness and lets miri exercise the
    // rest of the program.
    if cfg!(miri) {
        let _ = (ptr, len);
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use core::ffi::{c_int, c_void};
        // 4 KiB base page is universal on x86_64 / aarch64 Linux setups
        // kevy targets. (On systems using 16 KiB / 64 KiB pages the wider
        // alignment still happens to be a 4-KiB multiple, so this is
        // correct, just slightly more conservative.)
        const PAGE: usize = 4096;
        if len < PAGE * 2 {
            return;
        }
        let start = ptr as usize;
        let aligned_start = (start + PAGE - 1) & !(PAGE - 1);
        let end = start + len;
        if aligned_start >= end {
            return;
        }
        let aligned_len = (end - aligned_start) & !(PAGE - 1);
        if aligned_len < PAGE * 2 {
            return;
        }
        // Linux MADV_HUGEPAGE = 14 (mm/madvise.c, asm-generic/mman-common.h).
        const MADV_HUGEPAGE: c_int = 14;
        // SAFETY: ffi::madvise is a kernel advise call; it reads no Rust
        // memory, performs no writes, and is benign on error (EINVAL on
        // mis-aligned / unsupported kernels is what we want — no-op).
        unsafe {
            let _ = ffi::madvise(
                aligned_start as *mut c_void,
                aligned_len,
                MADV_HUGEPAGE,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ptr, len);
    }
}

/// 2 MiB — the x86_64 / aarch64 transparent-huge-page boundary.
#[cfg(target_os = "linux")]
const HUGE_PAGE: usize = 2 * 1024 * 1024;

/// Allocate `len` bytes via anonymous `mmap`, with the returned address
/// **2 MiB-aligned** AND the mapped length rounded up to a 2 MiB multiple.
/// Then calls `MADV_HUGEPAGE` on the returned region.
///
/// 2 MiB alignment is what transparent huge pages require for the kernel
/// to promote a region: the global allocator (jemalloc-like chunk
/// placement) puts even MB-scale allocations at 4 KiB-aligned addresses
/// inside its arenas, so `khugepaged` cannot find a 2 MiB-aligned
/// candidate to promote even with `advise_hugepage` set. Allocating
/// straight from `mmap` and explicitly aligning gives the kernel a
/// promotion target.
///
/// **Linux only**: off Linux this returns `None` (the caller is expected
/// to fall back to the global allocator). Returns `None` on `mmap`
/// failure too — the caller should not panic; fall back instead.
///
/// The returned pointer must be released via [`munmap_2mb`]; passing it
/// to `dealloc()` is UB.
/// # Examples
///
/// Both outcomes are the contract, and a caller must handle each: on Linux
/// a 2 MiB-aligned mapping it owns, and anywhere else `None`, meaning fall
/// back to the global allocator rather than fail.
///
/// ```
/// const MIB2: usize = 2 * 1024 * 1024;
/// let len = 4 * MIB2;
/// match kevy_madvise::mmap_anon_aligned_2mb(len) {
///     Some(p) => {
///         assert_eq!(p.as_ptr() as usize % MIB2, 0, "alignment is the point");
///         // SAFETY: `p` came from this call and has not been released.
///         unsafe { kevy_madvise::munmap_2mb(p, len) };
///     }
///     None => {} // not Linux, or the mapping failed: the fallback path
/// }
/// ```
///
/// A zero-length request is `None` rather than an empty mapping.
///
/// ```
/// assert!(kevy_madvise::mmap_anon_aligned_2mb(0).is_none());
/// ```
/// # Examples
///
/// ```
/// // `None` rather than a panic when the platform cannot serve it, so
/// // the caller keeps its own fallback rather than inheriting one.
/// if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(2 << 20) {
///     assert_eq!(p.as_ptr() as usize % (2 << 20), 0, "2 MiB aligned");
///     unsafe { kevy_madvise::munmap_2mb(p, 2 << 20) };
/// }
/// ```
pub fn mmap_anon_aligned_2mb(len: usize) -> Option<core::ptr::NonNull<u8>> {
    if cfg!(miri) || len == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        mmap_anon_aligned_2mb_linux(len)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = len;
        None
    }
}

/// Linux body of [`mmap_anon_aligned_2mb`].
#[cfg(target_os = "linux")]
fn mmap_anon_aligned_2mb_linux(len: usize) -> Option<core::ptr::NonNull<u8>> {
    use core::ffi::c_void;
    // Linux mmap flags (asm-generic/mman.h + sys/mman.h):
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const MAP_PRIVATE: i32 = 0x2;
    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
    let rounded = (len + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
    // Over-allocate by one HP so we can trim down to a 2 MiB-aligned
    // start — mmap returns page-aligned (4 KiB), not HP-aligned.
    let total = rounded.checked_add(HUGE_PAGE)?;
    // SAFETY: mmap is the canonical anonymous map; no Rust memory is
    // read or written. NULL addr lets the kernel pick.
    let raw = unsafe {
        ffi::mmap(
            core::ptr::null_mut(),
            total,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if raw == MAP_FAILED {
        return None;
    }
    let aligned_start = trim_to_aligned(raw, total, rounded);
    // Best-effort huge-page hint. EINVAL on unsupported kernels =
    // benign — the mapping still works at 4 KiB pages.
    const MADV_HUGEPAGE: i32 = 14;
    // SAFETY: `aligned_start..aligned_start+rounded` is fully mapped,
    // HP-aligned, HP-multiple. madvise reads no Rust memory.
    unsafe {
        let _ = ffi::madvise(aligned_start as *mut c_void, rounded, MADV_HUGEPAGE);
    }
    core::ptr::NonNull::new(aligned_start as *mut u8)
}

/// Trim the raw `total`-byte mapping at `raw` down to the 2 MiB-aligned
/// `rounded`-byte region, munmapping the unaligned prefix and the
/// trailing slack. Returns the aligned start address.
#[cfg(target_os = "linux")]
fn trim_to_aligned(raw: *mut core::ffi::c_void, total: usize, rounded: usize) -> usize {
    use core::ffi::c_void;
    let raw_addr = raw as usize;
    let aligned_start = (raw_addr + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
    let prefix = aligned_start - raw_addr;
    let suffix = total - prefix - rounded;
    // Trim the unaligned prefix.
    if prefix > 0 {
        // SAFETY: prefix bytes at `raw` are exactly what we just mapped.
        unsafe {
            ffi::munmap(raw, prefix);
        }
    }
    // Trim the trailing slack past the aligned region.
    if suffix > 0 {
        // SAFETY: `aligned_start + rounded` is inside the mapping.
        unsafe {
            ffi::munmap((aligned_start + rounded) as *mut c_void, suffix);
        }
    }
    aligned_start
}

/// Release a buffer previously returned by [`mmap_anon_aligned_2mb`].
/// `len` must equal the original allocation length (or any value within
/// the same 2 MiB-rounded total — the function rounds internally to match).
/// Passing a pointer NOT obtained from [`mmap_anon_aligned_2mb`] is UB.
///
/// **Linux only**; on other targets this is a compile-time no-op (the
/// caller should never have a non-None pointer to free).
///
/// # Safety
/// `ptr` must come from a successful [`mmap_anon_aligned_2mb`] call and
/// not yet have been munmap'd. `len` must match the original `len` arg.
/// # Examples
///
/// The pointer must be one [`mmap_anon_aligned_2mb`] returned, with the
/// same `len` — the mapping is released whole, and the rounding to a
/// 2 MiB multiple is redone here from `len` rather than remembered.
///
/// ```
/// let len = 4 * 1024 * 1024;
/// if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(len) {
///     // SAFETY: `p` is this call's own mapping, released exactly once.
///     unsafe { kevy_madvise::munmap_2mb(p, len) };
/// }
/// ```
/// # Examples
///
/// ```
/// // Pairs with `mmap_anon_aligned_2mb`, and only with it: `len` must be
/// // the length that mapping was made with.
/// if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(2 << 20) {
///     unsafe { kevy_madvise::munmap_2mb(p, 2 << 20) };
/// }
/// ```
pub unsafe fn munmap_2mb(ptr: core::ptr::NonNull<u8>, len: usize) {
    if cfg!(miri) {
        let _ = (ptr, len);
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use core::ffi::c_void;
        let rounded = (len + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
        // SAFETY: caller guarantees ptr is a live mapping of `rounded`
        // bytes from this module.
        unsafe {
            let _ = ffi::munmap(ptr.as_ptr() as *mut c_void, rounded);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ptr, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_call_below_two_pages() {
        // Smaller than 2 * 4 KiB: short-circuit, never reaches the syscall.
        // We cannot directly assert "no syscall" without a hook, but the
        // function must at least return cleanly on a tiny buffer.
        let buf = [0u8; 1024];
        advise_hugepage(buf.as_ptr(), buf.len());
    }

    #[test]
    fn unaligned_buffer_does_not_panic() {
        // 16 KiB unaligned buffer; the wrapper rounds inward and either
        // calls madvise on the aligned subset or no-ops. Either way, no
        // panic, no UB.
        let buf = vec![0u8; 16 * 1024];
        advise_hugepage(buf.as_ptr().wrapping_add(7), buf.len() - 7);
    }

    #[test]
    fn zero_length_is_noop() {
        advise_hugepage(core::ptr::null(), 0);
    }

    #[test]
    fn large_aligned_region_runs() {
        // 64 KiB region — enough to clear all the page-alignment guards.
        // On Linux this issues the syscall; on macOS it's compile-time
        // out. We only assert the function completes.
        let buf = vec![0u8; 64 * 1024];
        advise_hugepage(buf.as_ptr(), buf.len());
    }
}
