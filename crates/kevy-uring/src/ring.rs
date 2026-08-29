//! The io_uring engine: `IoUring::new` sets up the kernel ring + maps the
//! three shared regions; `prep_*` queues SQEs into the SQ; `submit_and_wait`
//! enters the kernel; `for_each_completion` reaps completed CQEs.

use core::ffi::{c_int, c_long, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::io;

use crate::completion::Completion;
use crate::ffi::{
    self, IORING_ENTER_GETEVENTS, IORING_ENTER_SQ_WAKEUP, IORING_SQ_NEED_WAKEUP,
    SYS_IO_URING_ENTER,
};
use crate::layout::IoUringSqe;

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
    /// Iterations since the last `io_uring_enter` syscall. The reactor
    /// calls `submit_and_wait(0)` every iter; if neither new SQEs were
    /// queued nor `wait_nr > 0`, the syscall does no useful work (just
    /// runs task_work for COOP_TASKRUN). Tracking this lets us skip the
    /// syscall for up to [`ENTER_SKIP_THRESHOLD`] empty iterations — a
    /// forced enter every N iters still flushes deferred task_work so
    /// completions don't stall.
    iters_since_enter: u32,
}

/// Maximum empty reactor iterations between forced `io_uring_enter`
/// syscalls, bounding the completion-delivery delay (task_work flush
/// under COOP_TASKRUN) to ~2 microseconds even on a quiet shard
/// (~1 M iters/s observed with one idle conn). Tuned across 2/4/16:
/// higher values save more syscalls but bleed a few % throughput on
/// the RTT-bound single-connection path (task_work delay shows up as
/// added reply latency), so the smallest effective value wins.
const ENTER_SKIP_THRESHOLD: u32 = 2;

// SAFETY: `IoUring` owns its fd and mappings exclusively; moving the whole
// engine to another thread (one per shard) is sound. It is not `Sync`
// (single owner).
unsafe impl Send for IoUring {}

impl IoUring {
    /// Create a ring sized for at least `entries` in-flight submissions.
    ///
    /// # Examples
    ///
    /// A no-op submitted and reaped — the smallest complete round trip
    /// through the ring, and the shape every other operation follows:
    /// queue SQEs, enter the kernel once, then drain the CQ.
    ///
    /// ```
    /// use kevy_uring::IoUring;
    ///
    /// // io_uring can be absent (pre-5.1) or refused by a seccomp policy,
    /// // which is why this returns a Result rather than panicking. A caller
    /// // with an epoll path takes it here; one without should report why.
    /// let Ok(mut ring) = IoUring::new(8) else { return };
    ///
    /// assert!(ring.prep_nop(0xbeef));            // queue it
    /// assert_eq!(ring.submit_and_wait(1).unwrap(), 1); // one syscall
    ///
    /// let mut seen = None;
    /// ring.for_each_completion(|c| seen = Some((c.user_data, c.res)));
    /// assert_eq!(seen, Some((0xbeef, 0)));       // tagged, and it succeeded
    /// ```
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
    /// as the shard threads (measured 2–15× throughput regression with
    /// 10 shards on a 16-core box). Reserved for
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
            sqes: sqes_map.cast::<IoUringSqe>(),
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
            iters_since_enter: 0,
        };
        // Best-effort: register the ring's own fd into the calling thread's
        // io_uring registered-rings table (Linux 5.18+). On success, subsequent
        // `submit_and_wait` syscalls reference the ring by index and the
        // kernel skips fget/fput on the ring fd per syscall. On older kernels
        // this fails with EINVAL — the raw fd path stays in use.
        ring.try_register_ring_fd();
        Ok(ring)
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
    // LOC-WAIVER: per-iter busy-poll submit hot body; bulk of the length is enter-skip contract comments.
    pub fn submit_and_wait(&mut self, wait_nr: u32) -> io::Result<u32> {
        // SAFETY: `sq_ktail` is the kernel-published tail ptr.
        let prev = unsafe { (*self.sq_ktail).load(Ordering::Relaxed) };
        let to_submit = self.sq_tail.wrapping_sub(prev);
        // SAFETY: publishing our local tail to the kernel-shared atomic.
        unsafe { (*self.sq_ktail).store(self.sq_tail, Ordering::Release) };

        // Threshold-based enter skip. A syscall-tracepoint diagnostic
        // showed ~12 wasted io_uring_enter calls per actual op on the
        // steady single-connection hot path. Skipping unconditionally
        // when to_submit==0 && wait_nr==0 regresses: COOP_TASKRUN
        // delays completion task_work until the next enter, so
        // never-entering stalls completions. Instead we skip up to
        // ENTER_SKIP_THRESHOLD empty iters in a row, then enter once
        // to flush task_work. The skip path is gated on the non-SQPOLL
        // case (SQPOLL has its own skip below) and on wait_nr == 0
        // (the caller doesn't need a completion to arrive).
        //
        // A `IORING_SETUP_TASKRUN_FLAG` variant (Linux 6.0+: the kernel
        // sets `IORING_SQ_TASKRUN` in sq_flags when task_work is
        // pending, letting userland skip the syscall whenever the bit
        // is clear) was tried and REVERTED: it regressed GET by ~30%
        // with multi-second stalls mid-test — the bit's set/clear
        // timing under COOP_TASKRUN doesn't match the busy-poll loop
        // closely enough to remain race-free, and even with this
        // counter as a safety net on top, a window remained where CQEs
        // piled up between bit-clear observations.
        if to_submit == 0 && wait_nr == 0 && self.sq_flags.is_none() {
            self.iters_since_enter = self.iters_since_enter.saturating_add(1);
            if self.iters_since_enter < ENTER_SKIP_THRESHOLD {
                return Ok(0);
            }
            // Reached the threshold — fall through to the syscall path
            // below so task_work flushes. Counter resets after syscall.
        }

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
            Some((idx, flag)) => (ffi::arg(idx), flag),
            None => (c_long::from(self.ring_fd), 0),
        };
        enter_flags |= extra_flags;
        // Retried on EINTR like every other syscall loop in the tree
        // (the pollers, the socket reads, the accept loop): a signal
        // landing mid-enter — SIGSTOP/SIGCONT included — must not kill
        // the shard. Re-entering with the same arguments is safe: the
        // kernel derives the submit count from the shared SQ cursors,
        // so nothing double-submits.
        let ret = loop {
            // SAFETY: kernel-validated args; no Rust memory is read/written.
            let r = unsafe {
                ffi::syscall(
                    SYS_IO_URING_ENTER,
                    syscall_fd,
                    ffi::arg(to_submit),
                    ffi::arg(wait_nr),
                    ffi::arg(enter_flags),
                    ptr::null::<c_void>(),
                    0usize,
                )
            };
            if r >= 0 {
                break r;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        };
        // Real enter happened — the skip counter resets.
        self.iters_since_enter = 0;
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
            ffi::munmap(self.sqes.cast::<c_void>(), self.sqes_len);
            ffi::munmap(self.cq_mmap, self.cq_mmap_len);
            ffi::munmap(self.sq_mmap, self.sq_mmap_len);
            ffi::close(self.ring_fd);
        }
    }
}
