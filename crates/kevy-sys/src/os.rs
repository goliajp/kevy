//! The C library's memory and process calls, declared once for every kevy
//! crate that needs them — the allocator, the huge-page advisor, the
//! io_uring binding and the test harness included. This module needs no
//! `std`, so `no_std` crates take it with `default-features = false`.
//!
//! Declarations only; each caller keeps its own constants and its own
//! `// SAFETY:` argument at the call.
//!
//! ```
//! # #[cfg(unix)] {
//! // SAFETY: sysconf reads a configuration value and touches no memory.
//! let page = unsafe { kevy_sys::os::sysconf(if cfg!(target_os = "macos") { 29 } else { 30 }) };
//! assert!(page >= 4096);
//! # }
//! ```

#[cfg(unix)]
use core::ffi::{c_int, c_void};

#[cfg(unix)]
unsafe extern "C" {
    /// `mmap(2)`.
    pub fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    /// `munmap(2)`.
    pub fn munmap(addr: *mut c_void, length: usize) -> c_int;
    /// `madvise(2)`.
    pub fn madvise(addr: *mut c_void, length: usize, advice: c_int) -> c_int;
    /// `sysconf(3)`.
    pub fn sysconf(name: c_int) -> i64;
    /// `close(2)`.
    pub fn close(fd: c_int) -> c_int;
    /// `setrlimit(2)`; `rlim` points at `{ rlim_cur: u64, rlim_max: u64 }`.
    pub fn setrlimit(resource: c_int, rlim: *const c_void) -> c_int;
}

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe extern "C" {
    /// `syscall(2)`, for calls without a C library wrapper (io_uring).
    /// Variadic in C.
    pub fn syscall(num: core::ffi::c_long, ...) -> core::ffi::c_long;
}
