//! A reaped completion event.

use crate::ffi::{
    IORING_CQE_BUFFER_SHIFT, IORING_CQE_F_BUFFER, IORING_CQE_F_MORE, IORING_CQE_F_SOCK_NONEMPTY,
};

/// One reaped completion (`struct io_uring_cqe`): the `user_data` you tagged
/// the submission with, and `res` (bytes transferred / accepted fd when ≥ 0,
/// else `-errno`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Completion {
    /// The `user_data` tag the submission carried.
    pub user_data: u64,
    /// Result of the op: bytes transferred or accepted fd (≥ 0) or `-errno`.
    pub res: i32,
    /// io_uring `flags` (provided-buffer id + multishot armed bit).
    pub flags: u32,
}

impl Completion {
    /// The provided-buffer id the kernel filled, if this completion consumed
    /// one (multishot/`recv` with buffer select). Recycle it via
    /// [`ProvidedBufRing::recycle`](crate::ProvidedBufRing::recycle) once the
    /// bytes are copied out.
    pub fn buffer_id(&self) -> Option<u16> {
        (self.flags & IORING_CQE_F_BUFFER != 0)
            .then_some((self.flags >> IORING_CQE_BUFFER_SHIFT) as u16)
    }

    /// Whether the originating multishot SQE remains armed (more completions
    /// to come). When `false`, the op terminated and must be re-submitted.
    pub fn has_more(&self) -> bool {
        self.flags & IORING_CQE_F_MORE != 0
    }

    /// Whether the socket still holds unread data after this completion
    /// (`IORING_CQE_F_SOCK_NONEMPTY`). On a multishot recv that
    /// terminated with `res == 0`, this bit is the difference between a
    /// real EOF (bit clear — the peer closed) and a spurious
    /// termination with bytes still queued (bit set — re-arm to drain
    /// them, do not close).
    pub fn sock_nonempty(&self) -> bool {
        self.flags & IORING_CQE_F_SOCK_NONEMPTY != 0
    }
}

// The CQE the ring is read through; `setup.rs` sizes the completion
// mmap from it. See `layout.rs` for why this is a build failure rather
// than a test.
const _: () = assert!(size_of::<Completion>() == 16, "io_uring_cqe is 16 bytes");
const _: () = assert!(align_of::<Completion>() == 8);
