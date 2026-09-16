//! `poll(2)` for a handful of descriptors: wait until one is readable.
//!
//! The server's reactors own their file descriptors and use the platform
//! poller; a command-line client waiting on a socket and its standard input
//! at once (redis-cli's pub/sub prompt does exactly that) needs the portable
//! call, not a reactor.

use crate::ffi;
use core::ffi::c_int;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

/// `POLLIN`: the same value on Linux and macOS.
const POLLIN: i16 = 0x1;

/// `struct pollfd`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct PollFd {
    pub(crate) fd: c_int,
    pub(crate) events: i16,
    pub(crate) revents: i16,
}

const _: () = assert!(size_of::<PollFd>() == 8, "struct pollfd is int + short + short");

/// Wait until any of `fds` is readable, or `timeout` passes.
///
/// Returns one flag per descriptor, in order: `true` when it is readable or
/// in an error/hang-up state (a read will not block and will say which).
/// An interrupted wait (`EINTR`) returns all `false`, like a timeout, so the
/// caller's loop re-checks whatever the signal changed.
///
/// # Examples
///
/// ```
/// use std::io::Write;
/// use std::os::fd::AsRawFd;
/// use std::os::unix::net::UnixStream;
/// use std::time::Duration;
///
/// let (mut tx, rx) = UnixStream::pair()?;
/// tx.write_all(b"ping")?;
/// let ready = kevy_sys::wait_readable(&[rx.as_raw_fd()], Duration::from_secs(1))?;
/// assert_eq!(ready, [true]);
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Errors
///
/// The `poll(2)` error other than `EINTR`, e.g. `EINVAL` for too many fds.
pub fn wait_readable(fds: &[RawFd], timeout: Duration) -> io::Result<Vec<bool>> {
    let mut set: Vec<PollFd> =
        fds.iter().map(|&fd| PollFd { fd, events: POLLIN, revents: 0 }).collect();
    let millis = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
    // SAFETY: `set` is a live, exclusively borrowed buffer of `set.len()`
    // `PollFd`s whose layout is checked against `struct pollfd` above; the
    // kernel writes only `revents` within that length. `nfds` is the same
    // length. No descriptor is closed or read by poll itself.
    let n = unsafe { ffi::poll(set.as_mut_ptr(), set.len() as ffi::NfdsT, millis) };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(vec![false; fds.len()]);
        }
        return Err(err);
    }
    Ok(set.iter().map(|p| p.revents != 0).collect())
}

#[cfg(test)]
mod tests {
    use super::wait_readable;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn reports_which_descriptor_has_data() {
        let (mut a, b) =
            UnixStream::pair().expect("socketpair has no failure mode in a test process");
        let (_c, d) = UnixStream::pair().expect("socketpair has no failure mode in a test process");
        assert_eq!(
            wait_readable(&[b.as_raw_fd(), d.as_raw_fd()], Duration::from_millis(10)).ok(),
            Some(vec![false, false])
        );
        a.write_all(b"x").expect("a fresh socketpair has buffer room for one byte");
        assert_eq!(
            wait_readable(&[b.as_raw_fd(), d.as_raw_fd()], Duration::from_millis(1000)).ok(),
            Some(vec![true, false])
        );
    }
}
