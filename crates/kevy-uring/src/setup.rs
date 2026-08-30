//! Ring construction: the `io_uring_setup` syscall, the three `mmap`'d
//! shared regions, and the cursors recovered from them.
//!
//! Split out of `ring.rs` when that file reached the workspace's 500-line
//! ceiling. The boundary is not arbitrary: everything here runs once, at
//! construction, and touches no `self` — it computes the region sizes,
//! maps them, and hands back the raw pointers that `IoUring::new_inner`
//! assembles into a ring. What stays in `ring.rs` is the part that runs
//! per iteration for the life of the process.

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::io;

use crate::completion::Completion;
use crate::ffi::{
    self, IORING_OFF_CQ_RING, IORING_OFF_SQ_RING, IORING_OFF_SQES, IORING_SETUP_COOP_TASKRUN,
    IORING_SETUP_SINGLE_ISSUER, IORING_SETUP_SQ_AFF, IORING_SETUP_SQPOLL, MAP_POPULATE, MAP_SHARED,
    PROT_READ, PROT_WRITE, SYS_IO_URING_SETUP,
};
use crate::layout::{IoUringParams, IoUringSqe};
use crate::ring::IoUring;

/// Cursors recovered from the SQ ring mapping.
pub(crate) struct SqCursors {
    pub(crate) khead: *const AtomicU32,
    pub(crate) ktail: *const AtomicU32,
    pub(crate) array: *mut u32,
    pub(crate) mask: u32,
    pub(crate) tail: u32,
    /// SQ flag word — `IORING_SQ_NEED_WAKEUP` lives here under SQPOLL.
    pub(crate) flags: *const AtomicU32,
}

/// Cursors recovered from the CQ ring mapping.
pub(crate) struct CqCursors {
    pub(crate) khead: *const AtomicU32,
    pub(crate) ktail: *const AtomicU32,
    pub(crate) cqes: *const Completion,
    pub(crate) mask: u32,
}

impl IoUring {
    /// `mmap` all three io_uring shared regions. On any failure, cleans up
    /// the partial state (close fd, unmap what was already mapped) and
    /// returns the original syscall error.
    pub(crate) fn map_three_regions(
        ring_fd: c_int,
        sq_len: usize,
        cq_len: usize,
        sqes_len: usize,
    ) -> io::Result<(*mut c_void, *mut c_void, *mut c_void)> {
        let sq_mmap = Self::map_region(ring_fd, sq_len, IORING_OFF_SQ_RING).inspect_err(|_| {
            // SAFETY: ring_fd came from setup; not yet observed elsewhere.
            unsafe { ffi::close(ring_fd) };
        })?;
        let cq_mmap = Self::map_region(ring_fd, cq_len, IORING_OFF_CQ_RING).inspect_err(|_| {
            // SAFETY: free what we mapped + close the fd.
            unsafe {
                ffi::munmap(sq_mmap, sq_len);
                ffi::close(ring_fd);
            }
        })?;
        let sqes_map = Self::map_region(ring_fd, sqes_len, IORING_OFF_SQES).inspect_err(|_| {
            // SAFETY: free what we mapped + close the fd.
            unsafe {
                ffi::munmap(cq_mmap, cq_len);
                ffi::munmap(sq_mmap, sq_len);
                ffi::close(ring_fd);
            }
        })?;
        Ok((sq_mmap, cq_mmap, sqes_map))
    }

    /// Issue `io_uring_setup` and return `(ring_fd, params)`. When `sqpoll`
    /// is `Some((idle_ms, cpu))`, configures the kernel-side SQ poll thread.
    ///
    /// For the non-SQPOLL path (the default kevy reactor) tries
    /// `IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_COOP_TASKRUN` first
    /// (Linux 6.0+, +3–5% measured) and falls back
    /// to a plain setup if the kernel rejects them (EINVAL). The fallback
    /// keeps Linux 5.13+ supported with no hard version check.
    ///
    /// **Not enabled**: `IORING_SETUP_DEFER_TASKRUN` (Linux 6.1+) — it
    /// changes the CQ ring semantics so completions only land after
    /// `io_uring_enter` is called. kevy's reactor busy-polls the CQ ring
    /// directly without entering the kernel on the steady state, so
    /// DEFER_TASKRUN starves completions (measured 65–73% regression
    /// when isolated).
    pub(crate) fn setup_ring(
        entries: u32,
        sqpoll: Option<(u32, Option<u32>)>,
    ) -> io::Result<(c_int, IoUringParams)> {
        // SQPOLL is mutually exclusive with the cooperative flags
        // (the SQ poll kernel thread is the one running task_work, not the
        // user thread). Otherwise prefer the strongest set the kernel
        // accepts; fall back on EINVAL by dropping flags level by level.
        let sqpoll_flags: u32 = match sqpoll {
            Some(_) => IORING_SETUP_SQPOLL,
            None => 0,
        };
        let modern_flag_tiers: &[u32] = if sqpoll.is_some() {
            &[0]
        } else {
            &[IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_COOP_TASKRUN, 0]
        };

        for &modern in modern_flag_tiers {
            let mut p = IoUringParams { flags: sqpoll_flags | modern, ..Default::default() };
            if let Some((idle_ms, cpu)) = sqpoll {
                p.sq_thread_idle = idle_ms;
                if let Some(c) = cpu {
                    p.flags |= IORING_SETUP_SQ_AFF;
                    p.sq_thread_cpu = c;
                }
            }
            // SAFETY: `&mut p` lives across this call; kernel writes via ptr.
            let fd = unsafe { ffi::syscall(SYS_IO_URING_SETUP, ffi::arg(entries), &raw mut p) };
            if fd >= 0 {
                return Ok((fd as c_int, p));
            }
            let err = io::Error::last_os_error();
            // EINVAL = kernel doesn't recognise these flags. Try next tier.
            if err.raw_os_error() != Some(22) {
                return Err(err);
            }
        }
        Err(io::Error::last_os_error())
    }

    /// Compute the three mapping lengths the kernel needs us to map.
    pub(crate) fn region_sizes(p: &IoUringParams) -> (usize, usize, usize) {
        let sq_len = (p.sq_off.array as usize) + (p.sq_entries as usize) * 4;
        let cq_len =
            (p.cq_off.cqes as usize) + (p.cq_entries as usize) * core::mem::size_of::<Completion>();
        let sqes_len = (p.sq_entries as usize) * core::mem::size_of::<IoUringSqe>();
        (sq_len, cq_len, sqes_len)
    }

    /// `mmap` one of the three io_uring regions (`MAP_SHARED | MAP_POPULATE`).
    pub(crate) fn map_region(ring_fd: c_int, len: usize, off: i64) -> io::Result<*mut c_void> {
        // SAFETY: kernel-validated `len`/`off`/`ring_fd`; null hint lets the
        // kernel pick the address. Returns -1 on failure.
        let m = unsafe {
            ffi::mmap(
                ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                ring_fd,
                off,
            )
        };
        if m as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(m)
    }

    /// Extract the SQ cursors from a just-mapped SQ region.
    ///
    /// # Safety
    /// `sq_mmap` must point to a region of at least
    /// `p.sq_off.array + p.sq_entries * 4` bytes, and the kernel must have
    /// filled `p.sq_off` for this ring.
    pub(crate) unsafe fn sq_cursors(sq_mmap: *mut c_void, p: &IoUringParams) -> SqCursors {
        let base = sq_mmap as usize;
        let at = |off: u32| (base + off as usize) as *const AtomicU32;
        let khead = at(p.sq_off.head);
        let ktail = at(p.sq_off.tail);
        let flags = at(p.sq_off.flags);
        let array = (base + p.sq_off.array as usize) as *mut u32;
        // SAFETY: caller's invariant says `ring_mask` is inside the region.
        let mask = unsafe { *((base + p.sq_off.ring_mask as usize) as *const u32) };
        // SAFETY: ktail is published by the kernel; reading current tail at
        // construction lets us start the local cursor in sync.
        let tail = unsafe { (*ktail).load(Ordering::Acquire) };
        SqCursors { khead, ktail, array, mask, tail, flags }
    }

    /// Extract the CQ cursors from a just-mapped CQ region.
    ///
    /// # Safety
    /// `cq_mmap` must point to a region of at least
    /// `p.cq_off.cqes + p.cq_entries * sizeof(Completion)` bytes.
    pub(crate) unsafe fn cq_cursors(cq_mmap: *mut c_void, p: &IoUringParams) -> CqCursors {
        let base = cq_mmap as usize;
        let at = |off: u32| (base + off as usize) as *const AtomicU32;
        let khead = at(p.cq_off.head);
        let ktail = at(p.cq_off.tail);
        let cqes = (base + p.cq_off.cqes as usize) as *const Completion;
        // SAFETY: caller's invariant says `ring_mask` is inside the region.
        let mask = unsafe { *((base + p.cq_off.ring_mask as usize) as *const u32) };
        CqCursors { khead, ktail, cqes, mask }
    }
}
