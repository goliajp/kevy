//! The io_uring engine: `IoUring::new` sets up the kernel ring + maps the
//! three shared regions; `prep_*` queues SQEs into the SQ; `submit_and_wait`
//! enters the kernel; `for_each_completion` reaps completed CQEs.

use core::ffi::{c_int, c_long, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::io;

use crate::completion::Completion;
use crate::ffi::{
    self, IORING_ENTER_GETEVENTS, IORING_OFF_CQ_RING, IORING_OFF_SQ_RING, IORING_OFF_SQES,
    IORING_OP_ACCEPT, IORING_OP_NOP, IORING_OP_READ, IORING_OP_RECV, IORING_OP_WRITE,
    IORING_RECV_MULTISHOT, IOSQE_BUFFER_SELECT, MAP_POPULATE, MAP_SHARED, PROT_READ, PROT_WRITE,
    SOCK_CLOEXEC, SOCK_NONBLOCK, SYS_IO_URING_ENTER, SYS_IO_URING_SETUP,
};
use crate::layout::{IoUringParams, IoUringSqe};
use crate::pbr::ProvidedBufRing;

/// A Linux io_uring instance: one submission ring + one completion ring.
pub struct IoUring {
    ring_fd: c_int,
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
        let (ring_fd, p) = Self::setup_ring(entries)?;
        let (sq_len, cq_len, sqes_len) = Self::region_sizes(&p);
        let (sq_mmap, cq_mmap, sqes_map) =
            Self::map_three_regions(ring_fd, sq_len, cq_len, sqes_len)?;

        // SAFETY: `sq_off` / `cq_off` were filled by the kernel for this ring;
        // their byte offsets lie inside the just-mapped regions.
        let sq = unsafe { Self::sq_cursors(sq_mmap, &p) };
        let cq = unsafe { Self::cq_cursors(cq_mmap, &p) };

        Ok(IoUring {
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
        })
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

    /// Issue `io_uring_setup` and return `(ring_fd, params)`.
    fn setup_ring(entries: u32) -> io::Result<(c_int, IoUringParams)> {
        let mut p = IoUringParams::default();
        // SAFETY: `&mut p` lives across this call; kernel writes via the ptr.
        let fd = unsafe {
            ffi::syscall(
                SYS_IO_URING_SETUP,
                entries as c_long,
                &mut p as *mut IoUringParams,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((fd as c_int, p))
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
        let array = (base + p.sq_off.array as usize) as *mut u32;
        // SAFETY: caller's invariant says `ring_mask` is inside the region.
        let mask = unsafe { *((base + p.sq_off.ring_mask as usize) as *const u32) };
        // SAFETY: ktail is published by the kernel; reading current tail at
        // construction lets us start the local cursor in sync.
        let tail = unsafe { (*ktail).load(Ordering::Acquire) };
        SqCursors { khead, ktail, array, mask, tail }
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
    fn reserve(&mut self) -> Option<usize> {
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

    /// Queue a `read(fd)` of `len` bytes into `buf`, tagged with `user_data`.
    /// Returns `false` if the SQ is full.
    ///
    /// # Safety
    /// `buf` must point to `len` writable bytes and stay valid until the matching
    /// completion is reaped.
    pub unsafe fn prep_read(&mut self, fd: i32, buf: *mut u8, len: u32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            ptr::write(
                self.sqes.add(idx),
                IoUringSqe::new(IORING_OP_READ, fd, buf as u64, len, user_data),
            );
        }
        true
    }

    /// Queue a `write(fd)` of `len` bytes from `buf`, tagged with `user_data`.
    /// Returns `false` if the SQ is full.
    ///
    /// # Safety
    /// `buf` must point to `len` readable bytes and stay valid until the matching
    /// completion is reaped.
    pub unsafe fn prep_write(&mut self, fd: i32, buf: *const u8, len: u32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            ptr::write(
                self.sqes.add(idx),
                IoUringSqe::new(IORING_OP_WRITE, fd, buf as u64, len, user_data),
            );
        }
        true
    }

    /// Queue a **multishot** `recv(fd)` that draws its destination buffer from
    /// the provided-buffer group `bgid` (see [`IoUring::register_buf_ring`]): one
    /// SQE re-fires a completion per arrival, the kernel picking + reporting a
    /// buffer id each time, until it terminates (error / `ENOBUFS`, signalled by
    /// [`Completion::has_more`] returning `false`). No per-recv SQE, no read
    /// buffer to keep alive. Returns `false` if the SQ is full.
    pub fn prep_recv_multishot(&mut self, fd: i32, bgid: u16, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes.add(idx);
            // addr/len 0: the buffer comes from the group, not from us.
            ptr::write(sqe, IoUringSqe::new(IORING_OP_RECV, fd, 0, 0, user_data));
            (*sqe).ioprio = IORING_RECV_MULTISHOT;
            (*sqe).flags = IOSQE_BUFFER_SELECT;
            // `buf_index` aliases `buf_group` in the kernel ABI.
            (*sqe).buf_index = bgid;
        }
        true
    }

    /// Queue an `accept` on `listen_fd`; the accepted fd arrives as the
    /// completion's `res` (already `O_NONBLOCK | O_CLOEXEC`). Returns `false` if
    /// the SQ is full.
    pub fn prep_accept(&mut self, listen_fd: i32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes.add(idx);
            ptr::write(
                sqe,
                IoUringSqe::new(IORING_OP_ACCEPT, listen_fd, 0, 0, user_data),
            );
            (*sqe).rw_flags = SOCK_NONBLOCK | SOCK_CLOEXEC;
        }
        true
    }

    /// Queue a no-op tagged with `user_data` (used to prove the round-trip).
    /// Returns `false` if the SQ is full.
    pub fn prep_nop(&mut self, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            ptr::write(
                self.sqes.add(idx),
                IoUringSqe::new(IORING_OP_NOP, -1, 0, 0, user_data),
            );
        }
        true
    }

    /// Publish queued submissions and enter the kernel, optionally waiting for
    /// `wait_nr` completions. Returns the number of SQEs consumed.
    pub fn submit_and_wait(&mut self, wait_nr: u32) -> io::Result<u32> {
        // SAFETY: `sq_ktail` is the kernel-published tail ptr.
        let prev = unsafe { (*self.sq_ktail).load(Ordering::Relaxed) };
        let to_submit = self.sq_tail.wrapping_sub(prev);
        // SAFETY: publishing our local tail to the kernel-shared atomic.
        unsafe { (*self.sq_ktail).store(self.sq_tail, Ordering::Release) };
        let flags = if wait_nr > 0 { IORING_ENTER_GETEVENTS } else { 0 };
        // SAFETY: kernel-validated args; no Rust memory is read/written.
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_ENTER,
                self.ring_fd as c_long,
                to_submit as c_long,
                wait_nr as c_long,
                flags as c_long,
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

    /// Register a **provided-buffer ring** of `entries` (power of two) buffers of
    /// `buf_size` bytes each under group id `bgid`, for multishot
    /// [`prep_recv_multishot`](Self::prep_recv_multishot). The kernel draws a
    /// buffer per arrival and reports its id; the application recycles it via
    /// [`ProvidedBufRing::recycle`]. The registration is auto-released when the
    /// ring fd closes; the returned handle also unregisters + unmaps on drop.
    pub fn register_buf_ring(
        &self,
        entries: u16,
        buf_size: u32,
        bgid: u16,
    ) -> io::Result<ProvidedBufRing> {
        ProvidedBufRing::new(self.ring_fd, entries, buf_size, bgid)
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
