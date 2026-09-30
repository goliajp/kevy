//! A reaped completion event.

use crate::ffi::{
    IORING_CQE_BUFFER_SHIFT, IORING_CQE_F_BUFFER, IORING_CQE_F_MORE, IORING_CQE_F_SOCK_NONEMPTY,
};

/// One reaped completion (`struct io_uring_cqe`): the `user_data` you tagged
/// the submission with, and `res` (bytes transferred / accepted fd when ≥ 0,
/// else `-errno`).
///
/// ```
/// let mut ring = kevy_uring::IoUring::new(8)?;
/// assert!(ring.prep_nop(42));
/// ring.submit_and_wait(1)?;
/// let mut reaped = Vec::new();
/// ring.for_each_completion(|c| reaped.push(c));
/// let c = reaped[0];
/// assert_eq!((c.user_data, c.res), (42, 0)); // our tag, and success
/// assert_eq!(c.buffer_id(), None, "a no-op draws no buffer");
/// assert!(!c.has_more(), "and is not multishot");
/// # Ok::<(), std::io::Error>(())
/// ```
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Completion {
    /// The `user_data` tag the submission carried.
    ///
    /// Completions arrive in whatever order the kernel finishes them; the
    /// tag is how each is matched back to its submission.
    ///
    /// ```
    /// let mut ring = kevy_uring::IoUring::new(8)?;
    /// assert!(ring.prep_nop(1) && ring.prep_nop(2));
    /// ring.submit_and_wait(2)?;
    /// let mut tags = Vec::new();
    /// ring.for_each_completion(|c| tags.push(c.user_data));
    /// tags.sort();
    /// assert_eq!(tags, [1, 2]);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub user_data: u64,
    /// Result of the op: bytes transferred or accepted fd (≥ 0) or `-errno`.
    ///
    /// ```
    /// use std::os::fd::AsRawFd;
    /// let mut ring = kevy_uring::IoUring::new(8)?;
    /// let (_reader, writer) = std::io::pipe()?;
    /// let msg = b"hello";
    /// // SAFETY: `msg` is static, so it outlives the completion reaped below.
    /// assert!(unsafe { ring.prep_write(writer.as_raw_fd(), msg.as_ptr(), 5, 1) });
    /// assert!(ring.prep_fsync(-1, 2)); // no such descriptor
    /// ring.submit_and_wait(2)?;
    /// let mut res = [0; 2];
    /// ring.for_each_completion(|c| res[c.user_data as usize - 1] = c.res);
    /// assert_eq!(res[0], 5, "bytes written");
    /// assert_eq!(res[1], -9, "-EBADF");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub res: i32,
    /// io_uring `flags` (provided-buffer id + multishot armed bit).
    ///
    /// The buffer id sits in the upper 16 bits, beside bit 0 saying there is
    /// one and bit 1 saying the multishot op is still armed.
    ///
    /// ```
    /// use kevy_uring::Completion;
    /// let c = Completion { flags: 3 << 16 | 0b11, ..Completion::default() };
    /// assert_eq!(c.buffer_id(), Some(3));
    /// assert!(c.has_more());
    /// assert_eq!(Completion::default().buffer_id(), None);
    /// ```
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
