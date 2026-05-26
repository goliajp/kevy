//! kevy-madvise — thin pure-Rust `madvise` hints.
//!
//! A single best-effort kernel hint: tell Linux a region is a candidate for
//! transparent huge pages (`MADV_HUGEPAGE`). Hand-bound with `unsafe extern
//! "C"` against glibc — no `libc` crate, no third-party dependency. Off Linux
//! every entry point compile-time no-ops.
//!
//! Carved out of `kevy-sys` so it can be used by stones (like `kevy-map`)
//! without dragging the rest of the OS-boundary cement along. See
//! [`advise_hugepage`] for the only entry point.
//!
//! # Safety
//!
//! `unsafe` is confined to a single `extern "C"` declaration of `madvise(3)`
//! and one wrapper call site. The wrapper rounds the request to page
//! boundaries, never reads or writes Rust memory, and silently no-ops when
//! the kernel returns `EINVAL` — making it safe to expose as a plain `fn`.

#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
mod ffi {
    use core::ffi::{c_int, c_void};

    // Memory advice (Linux). The only libc symbol kevy-madvise touches; the
    // wrapper below is the only call site. glibc resolves this via `std`'s
    // existing linkage — no extra link directive needed.
    unsafe extern "C" {
        pub fn madvise(addr: *mut c_void, length: usize, advice: c_int) -> c_int;
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
pub fn advise_hugepage(ptr: *const u8, len: usize) {
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
