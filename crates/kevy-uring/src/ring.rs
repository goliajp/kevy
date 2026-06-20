//! The io_uring engine: `IoUring::new` sets up the kernel ring + maps the
//! three shared regions; `prep_*` queues SQEs into the SQ; `submit_and_wait`
//! enters the kernel; `for_each_completion` reaps completed CQEs.

use core::ffi::{c_int, c_long, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::io;

use crate::completion::Completion;
use crate::ffi::{
    self, IORING_ENTER_GETEVENTS, IORING_ENTER_SQ_WAKEUP, IORING_OFF_CQ_RING, IORING_OFF_SQ_RING,
    IORING_OFF_SQES, IORING_SETUP_COOP_TASKRUN, IORING_SETUP_SINGLE_ISSUER, IORING_SETUP_SQ_AFF,
    IORING_SETUP_SQPOLL, IORING_SQ_NEED_WAKEUP, MAP_POPULATE, MAP_SHARED, PROT_READ, PROT_WRITE,
    SYS_IO_URING_ENTER, SYS_IO_URING_SETUP,
};
use crate::layout::{IoUringParams, IoUringSqe};

/// A Linux io_uring instance: one submission ring + one completion ring.
pub struct IoUring {
    pub(crate) ring_fd: c_int,
    sq_mmap: *mut c_void,
    sq_mmap_len: usize,
    cq_mmap: *mut c_void,
    cq_mmap_len: usize,
    sqes: *mut IoUringSqe,
    sqes_len: usize,
    sq_entries: u32,
    sq_mask: u32,
    /// Local producer cursor; published to the kernel on `submit`.
    sq_tail: u32,
    sq_khead: *const AtomicU32,
    sq_ktail: *const AtomicU32,
    sq_array: *mut u32,
    cq_mask: u32,
    cq_khead: *const AtomicU32,
    cq_ktail: *const AtomicU32,
    cqes: *const Completion,
    /// `*const AtomicU32` to the shared SQ flag word, **only** populated when
    /// the ring was set up with `IORING_SETUP_SQPOLL`. `None` => classic mode,
    /// always call `io_uring_enter` to submit; `Some` => check
    /// `IORING_SQ_NEED_WAKEUP` first and skip the syscall when the SQ poll
    /// thread is awake.
    sq_flags: Option<*const AtomicU32>,
    /// `(index, enter_flag)` for a successful registered-ring-fd setup. When
    /// `Some((i, _))`, `submit_and_wait` passes `i` as the syscall fd and
    /// ORs `IORING_ENTER_REGISTERED_RING` into the enter flags — the kernel
    /// resolves the ring via the registered-rings table, skipping
    /// `fget`/`fput` per syscall. `None` = raw `ring_fd` path.
    pub(crate) enter_ring: Option<(u32, u32)>,
}

// SAFETY: `IoUring` owns its fd and mappings exclusively; moving the whole
// engine to another thread (one per shard) is sound. It is not `Sync`
// (single owner).
unsafe impl Send for IoUring {}

/// Cursors recovered from the SQ ring mapping.
struct SqCursors {
    khead: *const AtomicU32,
    ktail: *const AtomicU32,
    array: *mut u32,
    mask: u32,
    tail: u32,
    /// SQ flag word — `IORING_SQ_NEED_WAKEUP` lives here under SQPOLL.
    flags: *const AtomicU32,
}

/// Cursors recovered from the CQ ring mapping.
struct CqCursors {
    khead: *const AtomicU32,
    ktail: *const AtomicU32,
    cqes: *const Completion,
    mask: u32,
}

impl IoUring {
    /// Create a ring sized for at least `entries` in-flight submissions.
    pub fn new(entries: u32) -> io::Result<IoUring> {
        Self::new_inner(entries, None)
    }

    /// Create a ring backed by a kernel-side **submission poll thread**
    /// (`IORING_SETUP_SQPOLL`). Submissions are reaped without an
    /// `io_uring_enter` syscall on the steady state; when the SQ poll
    /// thread parks (after `idle_ms` ms with no work), userland wakes it
    /// via [`Self::submit_and_wait`]'s SQ_WAKEUP path.
    ///
    /// `cpu = Some(c)` pins the kernel thread to CPU `c` via
    /// `IORING_SETUP_SQ_AFF`. Costs 1 core at ~100% whenever traffic
    /// flows; requires Linux 5.13+ (the version that dropped CAP_SYS_NICE
    /// for SQPOLL).
    ///
    /// **Not suitable for kevy's per-shard reactor.** Each ring spawns
    /// one kernel poll thread; in kevy's shared-nothing layout N shards
    /// would spawn N poll threads, each contending for the same cores
    /// as the shard threads (measured 2–15× throughput regression on
    /// the lx64 reference box, 10 shards on 16 cores — see
    /// `bench/PERF-ATTACK-LOG-2026-06-20.md` attack D5). Reserved for
    /// callers with a single-threaded reactor and an unallocated core
    /// budget for the kernel poll thread.
    pub fn new_sqpoll(entries: u32, idle_ms: u32, cpu: Option<u32>) -> io::Result<IoUring> {
        Self::new_inner(entries, Some((idle_ms, cpu)))
    }

    fn new_inner(entries: u32, sqpoll: Option<(u32, Option<u32>)>) -> io::Result<IoUring> {
        let (ring_fd, p) = Self::setup_ring(entries, sqpoll)?;
        let (sq_len, cq_len, sqes_len) = Self::region_sizes(&p);
        let (sq_mmap, cq_mmap, sqes_map) =
            Self::map_three_regions(ring_fd, sq_len, cq_len, sqes_len)?;

        // SAFETY: `sq_off` / `cq_off` were filled by the kernel for this ring;
        // their byte offsets lie inside the just-mapped regions.
        let sq = unsafe { Self::sq_cursors(sq_mmap, &p) };
        let cq = unsafe { Self::cq_cursors(cq_mmap, &p) };
        let sq_flags = if sqpoll.is_some() { Some(sq.flags) } else { None };

        let mut ring = IoUring {
            ring_fd,
            sq_mmap,
            sq_mmap_len: sq_len,
            cq_mmap,
            cq_mmap_len: cq_len,
            sqes: sqes_map as *mut IoUringSqe,
            sqes_len,
            sq_entries: p.sq_entries,
            sq_mask: sq.mask,
            sq_tail: sq.tail,
            sq_khead: sq.khead,
            sq_ktail: sq.ktail,
            sq_array: sq.array,
            cq_mask: cq.mask,
            cq_khead: cq.khead,
            cq_ktail: cq.ktail,
            cqes: cq.cqes,
            sq_flags,
            enter_ring: None,
        };
        // Best-effort: register the ring's own fd into the calling thread's
        // io_uring registered-rings table (Linux 5.18+). On success, subsequent
        // `submit_and_wait` syscalls reference the ring by index and the
        // kernel skips fget/fput on the ring fd per syscall. On older kernels
        // this fails with EINVAL — the raw fd path stays in use.
        ring.try_register_ring_fd();
        Ok(ring)
    }

    /// `mmap` all three io_uring shared regions. On any failure, cleans up
    /// the partial state (close fd, unmap what was already mapped) and
    /// returns the original syscall error.
    fn map_three_regions(
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
    /// (Linux 6.0+, +3–5% measured on the lx64 reference) and falls back
    /// to a plain setup if the kernel rejects them (EINVAL). The fallback
    /// keeps Linux 5.13+ supported with no hard version check.
    ///
    /// **Not enabled**: `IORING_SETUP_DEFER_TASKRUN` (Linux 6.1+) — it
    /// changes the CQ ring semantics so completions only land after
    /// `io_uring_enter` is called. kevy's reactor busy-polls the CQ ring
    /// directly without entering the kernel on the steady state, so
    /// DEFER_TASKRUN starves completions (measured 65–73% regression in
    /// the E2 isolation, see `bench/PERF-ATTACK-LOG-2026-06-20.md`).
    fn setup_ring(
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
            let mut p = IoUringParams::default();
            p.flags = sqpoll_flags | modern;
            if let Some((idle_ms, cpu)) = sqpoll {
                p.sq_thread_idle = idle_ms;
                if let Some(c) = cpu {
                    p.flags |= IORING_SETUP_SQ_AFF;
                    p.sq_thread_cpu = c;
                }
            }
            // SAFETY: `&mut p` lives across this call; kernel writes via ptr.
            let fd = unsafe {
                ffi::syscall(
                    SYS_IO_URING_SETUP,
                    entries as c_long,
                    &mut p as *mut IoUringParams,
                )
            };
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
    fn region_sizes(p: &IoUringParams) -> (usize, usize, usize) {
        let sq_len = (p.sq_off.array as usize) + (p.sq_entries as usize) * 4;
        let cq_len =
            (p.cq_off.cqes as usize) + (p.cq_entries as usize) * core::mem::size_of::<Completion>();
        let sqes_len = (p.sq_entries as usize) * core::mem::size_of::<IoUringSqe>();
        (sq_len, cq_len, sqes_len)
    }

    /// `mmap` one of the three io_uring regions (`MAP_SHARED | MAP_POPULATE`).
    fn map_region(ring_fd: c_int, len: usize, off: i64) -> io::Result<*mut c_void> {
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
    unsafe fn sq_cursors(sq_mmap: *mut c_void, p: &IoUringParams) -> SqCursors {
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
    unsafe fn cq_cursors(cq_mmap: *mut c_void, p: &IoUringParams) -> CqCursors {
        let base = cq_mmap as usize;
        let at = |off: u32| (base + off as usize) as *const AtomicU32;
        let khead = at(p.cq_off.head);
        let ktail = at(p.cq_off.tail);
        let cqes = (base + p.cq_off.cqes as usize) as *const Completion;
        // SAFETY: caller's invariant says `ring_mask` is inside the region.
        let mask = unsafe { *((base + p.cq_off.ring_mask as usize) as *const u32) };
        CqCursors { khead, ktail, cqes, mask }
    }

    /// Reserve the next SQ slot (advancing the producer cursor + array map);
    /// returns its SQE index, or `None` if the submission queue is full.
    /// Called from the `prep_*` helpers in [`crate::prep`].
    pub(crate) fn reserve(&mut self) -> Option<usize> {
        // SAFETY: `sq_khead` is the kernel-published head ptr.
        let khead = unsafe { (*self.sq_khead).load(Ordering::Acquire) };
        if self.sq_tail.wrapping_sub(khead) >= self.sq_entries {
            return None; // SQ full
        }
        let idx = (self.sq_tail & self.sq_mask) as usize;
        // The SQ `array` maps a ring slot to an SQE index (here 1:1).
        // SAFETY: `idx < sq_entries` ensures we're inside `sq_array`.
        unsafe { *self.sq_array.add(idx) = idx as u32 };
        self.sq_tail = self.sq_tail.wrapping_add(1);
        Some(idx)
    }

    /// Raw SQE table pointer — exposed for the `prep_*` helpers in
    /// [`crate::prep`]. Returned slot `idx` must come from `reserve()`.
    #[inline]
    pub(crate) fn sqes_ptr(&mut self) -> *mut IoUringSqe {
        self.sqes
    }

    /// Publish queued submissions and enter the kernel, optionally waiting for
    /// `wait_nr` completions. Returns the number of SQEs consumed.
    ///
    /// **SQPOLL fast path**: when the ring was constructed via
    /// [`Self::new_sqpoll`] and the SQ poll thread is awake
    /// (`IORING_SQ_NEED_WAKEUP` clear) and the caller doesn't need to block
    /// on completions (`wait_nr == 0`), we publish the tail and return
    /// **without any syscall** — the kernel thread will reap submissions on
    /// its next poll spin.
    pub fn submit_and_wait(&mut self, wait_nr: u32) -> io::Result<u32> {
        // SAFETY: `sq_ktail` is the kernel-published tail ptr.
        let prev = unsafe { (*self.sq_ktail).load(Ordering::Relaxed) };
        let to_submit = self.sq_tail.wrapping_sub(prev);
        // SAFETY: publishing our local tail to the kernel-shared atomic.
        unsafe { (*self.sq_ktail).store(self.sq_tail, Ordering::Release) };

        // **E3 — DROPPED** (measured 16-25% regression on lx64). Skipping
        // io_uring_enter on `to_submit == 0 && wait_nr == 0` conflicts with
        // the `IORING_SETUP_COOP_TASKRUN` flag enabled in E2 — the kernel
        // cooperative model needs userland to enter periodically so
        // task_work runs and CQEs flush. Detail in
        // `bench/PERF-ATTACK-LOG-2026-06-20.md` attack E3.

        let mut enter_flags = if wait_nr > 0 { IORING_ENTER_GETEVENTS } else { 0 };
        if let Some(sq_flags_ptr) = self.sq_flags {
            // SAFETY: `sq_flags_ptr` lives inside the SQ mmap, valid for ring
            // lifetime. Kernel writes IORING_SQ_NEED_WAKEUP on park; Acquire
            // pairs with the kernel's Release on update.
            let sq_flags = unsafe { (*sq_flags_ptr).load(Ordering::Acquire) };
            if sq_flags & IORING_SQ_NEED_WAKEUP != 0 {
                enter_flags |= IORING_ENTER_SQ_WAKEUP;
            } else if wait_nr == 0 {
                // SQ poll thread is awake and caller doesn't need to wait —
                // skip the syscall entirely. This is the SQPOLL fast path.
                return Ok(to_submit);
            }
        }
        // E1.5: when the ring is self-registered (IORING_REGISTER_RING_FDS),
        // pass the registered index instead of the raw fd. The kernel skips
        // its per-syscall fget/fput on the ring.
        let (syscall_fd, extra_flags) = match self.enter_ring {
            Some((idx, flag)) => (idx as c_long, flag),
            None => (self.ring_fd as c_long, 0),
        };
        enter_flags |= extra_flags;
        // SAFETY: kernel-validated args; no Rust memory is read/written.
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_ENTER,
                syscall_fd,
                to_submit as c_long,
                wait_nr as c_long,
                enter_flags as c_long,
                ptr::null::<c_void>(),
                0usize,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as u32)
    }

    /// Reap every available completion, calling `f` for each; returns the count.
    pub fn for_each_completion<F: FnMut(Completion)>(&mut self, mut f: F) -> u32 {
        // SAFETY: cq_khead / cq_ktail are the kernel-shared cursors.
        let mut head = unsafe { (*self.cq_khead).load(Ordering::Relaxed) };
        let tail = unsafe { (*self.cq_ktail).load(Ordering::Acquire) };
        let mut n = 0;
        while head != tail {
            let idx = (head & self.cq_mask) as usize;
            // SAFETY: `idx < cq_entries` by mask; cqes points to that array.
            let cqe = unsafe { *self.cqes.add(idx) };
            f(cqe);
            head = head.wrapping_add(1);
            n += 1;
        }
        // SAFETY: publish the consumer head to the kernel.
        unsafe { (*self.cq_khead).store(head, Ordering::Release) };
        n
    }


}

impl Drop for IoUring {
    fn drop(&mut self) {
        // SAFETY: each pointer is the matching `mmap` return; fd is ours.
        unsafe {
            ffi::munmap(self.sqes as *mut c_void, self.sqes_len);
            ffi::munmap(self.cq_mmap, self.cq_mmap_len);
            ffi::munmap(self.sq_mmap, self.sq_mmap_len);
            ffi::close(self.ring_fd);
        }
    }
}
