//! SQE preparation helpers — `prep_*` queue one io_uring submission entry
//! into the SQ ring. Split out of [`crate::ring`] so that file stays under
//! the 500-LOC house rule. Each helper returns `false` if the SQ is full;
//! the caller is expected to submit and retry.

use core::ptr;

use crate::ffi::{
    IORING_ACCEPT_MULTISHOT, IORING_OP_ACCEPT, IORING_OP_NOP, IORING_OP_READ, IORING_OP_RECV,
    IORING_OP_TIMEOUT, IORING_OP_WRITE, IORING_OP_WRITEV, IORING_RECV_MULTISHOT,
    IOSQE_BUFFER_SELECT, IOSQE_FIXED_FILE, Iovec, SOCK_CLOEXEC, SOCK_NONBLOCK,
};
use crate::layout::{IoUringSqe, KernelTimespec};
use crate::ring::IoUring;

impl IoUring {
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
                self.sqes_ptr().add(idx),
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
                self.sqes_ptr().add(idx),
                IoUringSqe::new(IORING_OP_WRITE, fd, buf as u64, len, user_data),
            );
        }
        true
    }

    /// Queue a `writev(fd, iov, iovcnt)`. L1 (2026-06-21): the reactor's
    /// reply path uses this to fuse [header iovec, value-borrow iovec,
    /// CRLF iovec] into one syscall — the per-GET memcpy of the value
    /// into the conn output buffer is avoided.
    ///
    /// # Safety
    /// `iov` must point to `iovcnt` valid `Iovec` entries, each `iov_base`
    /// pointing to a readable byte range of length `iov_len`. The kernel
    /// reads the iovec array AND each base asynchronously — both must
    /// stay valid until the matching completion is reaped (the reactor
    /// parks them in the conn's pending-writev state and drops on CQE).
    pub unsafe fn prep_writev(
        &mut self,
        fd: i32,
        iov: *const Iovec,
        iovcnt: u32,
        user_data: u64,
    ) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own
        // alone. addr = iov pointer; len = iovcnt; off field (unused here)
        // stays 0.
        unsafe {
            ptr::write(
                self.sqes_ptr().add(idx),
                IoUringSqe::new(IORING_OP_WRITEV, fd, iov as u64, iovcnt, user_data),
            );
        }
        true
    }

    /// Queue a **multishot** `recv(fd)` that draws its destination buffer from
    /// the provided-buffer group `bgid` (see [`IoUring::register_buf_ring`]): one
    /// SQE re-fires a completion per arrival, the kernel picking + reporting a
    /// buffer id each time, until it terminates (error / `ENOBUFS`, signalled by
    /// [`crate::Completion::has_more`] returning `false`). No per-recv SQE, no read
    /// buffer to keep alive. Returns `false` if the SQ is full.
    pub fn prep_recv_multishot(&mut self, fd: i32, bgid: u16, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes_ptr().add(idx);
            // addr/len 0: the buffer comes from the group, not from us.
            ptr::write(sqe, IoUringSqe::new(IORING_OP_RECV, fd, 0, 0, user_data));
            (*sqe).ioprio = IORING_RECV_MULTISHOT;
            (*sqe).flags = IOSQE_BUFFER_SELECT;
            // `buf_index` aliases `buf_group` in the kernel ABI.
            (*sqe).buf_index = bgid;
        }
        true
    }

    /// Same as [`Self::prep_write`] but addresses the destination by
    /// registered-files **slot index** instead of raw fd. Sets
    /// `IOSQE_FIXED_FILE`; the kernel skips its per-op `fget`/`fput`. Caller
    /// must have populated `slot` via
    /// [`crate::IoUring::update_file_slot`].
    ///
    /// # Safety
    /// Same as `prep_write`.
    pub unsafe fn prep_write_fixed(
        &mut self,
        slot: u32,
        buf: *const u8,
        len: u32,
        user_data: u64,
    ) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes_ptr().add(idx);
            ptr::write(
                sqe,
                IoUringSqe::new(IORING_OP_WRITE, slot as i32, buf as u64, len, user_data),
            );
            (*sqe).flags = IOSQE_FIXED_FILE;
        }
        true
    }

    /// Same as [`Self::prep_recv_multishot`] but addresses the source by
    /// registered-files **slot index** instead of raw fd. Sets
    /// `IOSQE_FIXED_FILE | IOSQE_BUFFER_SELECT`; the kernel skips its
    /// per-op `fget`/`fput`. Caller must have populated `slot` via
    /// [`crate::IoUring::update_file_slot`].
    pub fn prep_recv_multishot_fixed(
        &mut self,
        slot: u32,
        bgid: u16,
        user_data: u64,
    ) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes_ptr().add(idx);
            ptr::write(
                sqe,
                IoUringSqe::new(IORING_OP_RECV, slot as i32, 0, 0, user_data),
            );
            (*sqe).ioprio = IORING_RECV_MULTISHOT;
            (*sqe).flags = IOSQE_BUFFER_SELECT | IOSQE_FIXED_FILE;
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
            let sqe = self.sqes_ptr().add(idx);
            ptr::write(
                sqe,
                IoUringSqe::new(IORING_OP_ACCEPT, listen_fd, 0, 0, user_data),
            );
            (*sqe).rw_flags = SOCK_NONBLOCK | SOCK_CLOEXEC;
        }
        true
    }

    /// Queue a **multishot** accept on `listen_fd` (Linux 5.19+). The kernel
    /// keeps one SQE armed across many connections — each new fd arrives as
    /// its own CQE with `IORING_CQE_F_MORE` set in `flags` while still armed.
    /// When `F_MORE` is clear the multishot has terminated and userland must
    /// re-arm via this fn (or fall back to [`Self::prep_accept`]). Caller
    /// must keep `user_data` stable across the run of one multishot — each
    /// CQE replays the same tag.
    ///
    /// B4 (2026-06-20): replaces the one-SQE-per-accept call site in
    /// `kevy_rt::uring_reactor`. At -c1 (one persistent conn) zero
    /// difference; under high-conn-churn workloads cuts an SQE + an
    /// `arm_conns`-loop trip per accept.
    pub fn prep_accept_multishot(&mut self, listen_fd: i32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes_ptr().add(idx);
            ptr::write(
                sqe,
                IoUringSqe::new(IORING_OP_ACCEPT, listen_fd, 0, 0, user_data),
            );
            (*sqe).rw_flags = SOCK_NONBLOCK | SOCK_CLOEXEC;
            (*sqe).ioprio = IORING_ACCEPT_MULTISHOT;
        }
        true
    }

    /// Queue a relative timeout: the completion (res = `-ETIME`) arrives once
    /// `ts` elapses, or earlier with res = 0 / `-ECANCELED` if the ring shuts
    /// down. Bounds a blocking [`IoUring::submit_and_wait`] the way a poller's
    /// wait-timeout would. Returns `false` if the SQ is full.
    ///
    /// # Safety
    /// `ts` must stay valid (not moved or dropped) until the matching
    /// completion is reaped — the kernel reads it asynchronously.
    pub unsafe fn prep_timeout(&mut self, ts: *const KernelTimespec, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes_ptr().add(idx);
            // addr = timespec ptr, len = 1 (one timespec), off = 0 (pure
            // timeout — no completion-count trigger), rw_flags = 0 (relative).
            ptr::write(
                sqe,
                IoUringSqe::new(IORING_OP_TIMEOUT, -1, ts as u64, 1, user_data),
            );
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
                self.sqes_ptr().add(idx),
                IoUringSqe::new(IORING_OP_NOP, -1, 0, 0, user_data),
            );
        }
        true
    }
}
