//! `io_uring_register` helpers — provided-buffer ring, sparse registered-files
//! table, file-slot updates, and ring-fd self-register. Split out of
//! [`crate::ring`] so that file stays under the 500-LOC house rule. Each
//! method is on the same `impl IoUring` and accesses `ring_fd` /
//! `enter_ring` (pub(crate) fields) directly.

use core::ffi::c_long;

use crate::ffi::{
    self, IORING_ENTER_REGISTERED_RING, IORING_REGISTER_FILES2, IORING_REGISTER_FILES_UPDATE,
    IORING_REGISTER_RING_FDS, SYS_IO_URING_REGISTER,
};
use crate::layout::{
    IORING_RSRC_REGISTER_SPARSE, IoUringFilesUpdate, IoUringRsrcRegister, IoUringRsrcUpdate,
};
use crate::pbr::ProvidedBufRing;
use crate::ring::IoUring;
use std::io;

impl IoUring {
    /// Register a **provided-buffer ring** of `entries` (power of two) buffers of
    /// `buf_size` bytes each under group id `bgid`, for multishot
    /// `prep_recv_multishot`. The kernel draws a buffer per arrival and
    /// reports its id; the application recycles it via
    /// [`ProvidedBufRing::recycle`]. The registration is auto-released when
    /// the ring fd closes; the returned handle also unregisters + unmaps on
    /// drop.
    pub fn register_buf_ring(
        &self,
        entries: u16,
        buf_size: u32,
        bgid: u16,
    ) -> io::Result<ProvidedBufRing> {
        ProvidedBufRing::new(self.ring_fd, entries, buf_size, bgid)
    }

    /// Self-register `self.ring_fd` into the **current thread's** io_uring
    /// registered-rings table (`IORING_REGISTER_RING_FDS`, Linux 5.18+) and
    /// flip the `enter_ring` mode on success. Future `submit_and_wait`
    /// syscalls then pass the table index + `IORING_ENTER_REGISTERED_RING`
    /// instead of the raw fd, skipping the kernel's per-syscall
    /// `fget`/`fput` on the ring. Failure (older kernel / disabled feature)
    /// silently leaves `enter_ring = None`; behavior unchanged.
    ///
    /// **Thread affinity caveat**: registered-rings entries are per
    /// **thread**, not per process. The ring must be moved to the thread
    /// that will call `submit_and_wait` before this registration; in kevy
    /// that already holds — the shard's `run_uring` owns the ring and
    /// stays on one OS thread for the reactor's life.
    pub(crate) fn try_register_ring_fd(&mut self) {
        let mut upd = IoUringRsrcUpdate {
            offset: u32::MAX,
            resv: 0,
            data: self.ring_fd as u32 as u64,
        };
        // SAFETY: `upd` lives through the syscall; ring_fd is valid.
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_REGISTER,
                self.ring_fd as c_long,
                IORING_REGISTER_RING_FDS as c_long,
                &mut upd as *mut _ as c_long,
                1 as c_long,
            )
        };
        if ret == 1 {
            self.enter_ring = Some((upd.offset, IORING_ENTER_REGISTERED_RING));
        }
    }

    /// Register a **sparse registered-files table** of `nr` slots — empty
    /// initially; slots are filled later via [`Self::update_file_slot`] and
    /// referenced from SQEs by setting `IOSQE_FIXED_FILE` and putting the
    /// slot index in the SQE's `fd` field. The kernel skips the per-op
    /// `fget`/`fput` fd-table lookup for any SQE that uses a fixed file.
    ///
    /// Requires Linux 5.13+ for the rsrc-struct API. The table is
    /// auto-released when the ring fd closes. The kernel rejects
    /// `nr > RLIMIT_NOFILE`; callers that need >1024 slots must bump
    /// ulimit themselves.
    ///
    /// **Call shape**: `IORING_REGISTER_FILES2` (#13) with
    /// `IoUringRsrcRegister { nr, flags: IORING_RSRC_REGISTER_SPARSE, .. }`
    /// + `nr_args = sizeof::<IoUringRsrcRegister>() = 32`. There is no
    /// stand-alone "FILES_SPARSE" opcode in mainline.
    pub fn register_files_sparse(&self, nr: u32) -> io::Result<()> {
        let reg = IoUringRsrcRegister {
            nr,
            flags: IORING_RSRC_REGISTER_SPARSE,
            ..Default::default()
        };
        // SAFETY: `reg` lives through the syscall; ring_fd is valid.
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_REGISTER,
                self.ring_fd as c_long,
                IORING_REGISTER_FILES2 as c_long,
                &reg as *const _ as c_long,
                core::mem::size_of::<IoUringRsrcRegister>() as c_long,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Place `fd` into registered-files slot `index`, or unmap the slot if
    /// `fd < 0`. After a successful call SQEs referencing `IOSQE_FIXED_FILE`
    /// + `fd = index` will skip the fd-table lookup. Cheaper than the full
    /// per-op fget/fput once amortised across many ops per conn.
    pub fn update_file_slot(&self, index: u32, fd: i32) -> io::Result<()> {
        let fd_val: i32 = fd;
        let upd = IoUringFilesUpdate {
            offset: index,
            resv: 0,
            fds: (&fd_val as *const i32) as u64,
        };
        // SAFETY: `upd` and `fd_val` both live through the syscall; ring_fd
        // came from io_uring_setup.
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_REGISTER,
                self.ring_fd as c_long,
                IORING_REGISTER_FILES_UPDATE as c_long,
                &upd as *const _ as c_long,
                1 as c_long,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
