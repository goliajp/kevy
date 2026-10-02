//! A bounded wait: block in the ring until a completion arrives or a
//! deadline passes, without queueing a timeout operation. Split out of
//! [`crate::ring`] under the 500-line ceiling.

use core::ffi::c_void;
use core::time::Duration;
use std::io;

use crate::ffi::{self, IORING_ENTER_EXT_ARG, IORING_ENTER_GETEVENTS, SYS_IO_URING_ENTER};
use crate::layout::KernelTimespec;
use crate::ring::IoUring;

/// `struct io_uring_getevents_arg`.
#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    min_wait_usec: u32,
    ts: u64,
}

impl IoUring {
    /// Submit what is queued and block until a completion arrives or
    /// `timeout` passes, whichever is first. A timeout and a signal both
    /// return `Ok`: the caller looks at the completion queue either way.
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// let Ok(mut ring) = kevy_uring::IoUring::new(8) else { return };
    /// let t = Instant::now();
    /// ring.wait_timeout(Duration::from_millis(5)).unwrap();
    /// assert!(t.elapsed() >= Duration::from_millis(5), "nothing came, so it waited");
    /// ```
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        let to_submit = self.publish_tail();
        let (fd, fd_flag) = self.enter_fd();
        let ts = KernelTimespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        let arg =
            GeteventsArg { sigmask: 0, sigmask_sz: 0, min_wait_usec: 0, ts: &raw const ts as u64 };
        let flags = IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG | fd_flag;
        // SAFETY: `arg` and the timespec it points to outlive the call; the
        // kernel only reads them.
        let r = unsafe {
            ffi::syscall(
                SYS_IO_URING_ENTER,
                fd,
                ffi::arg(to_submit),
                ffi::arg(1u32),
                ffi::arg(flags),
                (&raw const arg).cast::<c_void>(),
                size_of::<GeteventsArg>(),
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            // ETIME: the deadline passed; EINTR: a signal. Neither is a failure.
            if !matches!(e.raw_os_error(), Some(62 | 4)) {
                return Err(e);
            }
        }
        self.entered()
    }
}
