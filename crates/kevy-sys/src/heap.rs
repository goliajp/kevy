//! The system allocator's own account of the heap: what it has handed out
//! and what it holds from the OS.
//!
//! - **Linux, glibc**: `mallinfo2(3)`, summed over every arena.
//! - **macOS**: `malloc_zone_statistics(3)` over every zone.
//!
//! Both are bound by hand (0-dep charter). Anything else answers `None`:
//! musl and other allocators publish no equivalent.

/// What the system allocator reports holding, in bytes.
///
/// Built by [`heap_stats`]; the fields are for reading.
///
/// ```
/// if let Some(h) = kevy_sys::heap_stats() {
///     // the free lists are what separates the two
///     assert!(h.in_use <= h.held);
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct HeapStats {
    /// Bytes in blocks handed out and not yet freed, the allocator's
    /// per-block overhead included, and the blocks it mapped on their own.
    ///
    /// ```
    /// let before = kevy_sys::heap_stats();
    /// let block = vec![1u8; 32 << 20];
    /// if let (Some(b), Some(a)) = (before, kevy_sys::heap_stats()) {
    ///     assert!(a.in_use >= b.in_use + (32 << 20), "the block is in use");
    /// }
    /// drop(block);
    /// ```
    pub in_use: u64,
    /// Bytes the allocator holds from the OS: [`Self::in_use`] plus the
    /// freed blocks it keeps for reuse. An upper bound on what the heap
    /// keeps resident, not a measure of it — the allocator may hold pages
    /// it has handed back with `madvise`, and memory it was given but no
    /// one touched is not resident.
    ///
    /// ```
    /// if let Some(h) = kevy_sys::heap_stats() {
    ///     assert!(h.held >= h.in_use);
    /// }
    /// ```
    pub held: u64,
}

/// The system allocator's statistics, or `None` where the platform's
/// allocator publishes none (anything but glibc and macOS).
///
/// **Not cheap**: glibc counts its free lists by walking them, so the
/// cost grows with the number of free blocks — milliseconds on a heap
/// fragmented into hundreds of thousands of them — and each arena is
/// locked while it is walked. Call it off the request path. A process
/// that installs its own global allocator gets the system allocator's
/// view, which then covers only what bypasses its own.
///
/// # Examples
///
/// ```
/// let stats = kevy_sys::heap_stats();
/// if cfg!(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")) {
///     assert!(stats.is_some_and(|h| h.in_use > 0), "a running program has a heap");
/// } else {
///     assert_eq!(stats, None);
/// }
/// ```
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn heap_stats() -> Option<HeapStats> {
    #[repr(C)]
    struct Mallinfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }
    unsafe extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: `mallinfo2` takes no arguments and returns its struct by
    // value; the layout above is glibc's `struct mallinfo2` (ten size_t,
    // malloc.h since 2.33), so nothing is read past what it writes.
    let m = unsafe { mallinfo2() };
    Some(HeapStats { in_use: (m.uordblks + m.hblkhd) as u64, held: (m.arena + m.hblkhd) as u64 })
}

/// See the glibc twin above.
///
/// # Examples
///
/// ```
/// let stats = kevy_sys::heap_stats();
/// if cfg!(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")) {
///     assert!(stats.is_some_and(|h| h.in_use > 0), "a running program has a heap");
/// } else {
///     assert_eq!(stats, None);
/// }
/// ```
#[cfg(target_os = "macos")]
pub fn heap_stats() -> Option<HeapStats> {
    #[repr(C)]
    #[derive(Default)]
    struct MallocStatistics {
        blocks_in_use: core::ffi::c_uint,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    unsafe extern "C" {
        fn malloc_zone_statistics(zone: *mut core::ffi::c_void, stats: *mut MallocStatistics);
    }
    let mut s = MallocStatistics::default();
    // SAFETY: a null zone asks for the sum over every zone
    // (malloc_zone_statistics(3)); `s` has `malloc_statistics_t`'s layout
    // and is written, not read.
    unsafe { malloc_zone_statistics(core::ptr::null_mut(), &raw mut s) };
    Some(HeapStats { in_use: s.size_in_use as u64, held: s.size_allocated as u64 })
}

/// No statistics on this platform's allocator.
///
/// # Examples
///
/// ```
/// let stats = kevy_sys::heap_stats();
/// if cfg!(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")) {
///     assert!(stats.is_some_and(|h| h.in_use > 0), "a running program has a heap");
/// } else {
///     assert_eq!(stats, None);
/// }
/// ```
#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
pub fn heap_stats() -> Option<HeapStats> {
    None
}
