//! Ctrl-C for an interactive client: either end the process, or cut one
//! connection loose and let the program carry on.
//!
//! A line-oriented client wants both. While it streams from a server
//! (subscribed, monitoring) Ctrl-C should stop the stream and return to the
//! prompt; anywhere else it should end the program. The decision has to be
//! made inside the signal handler, because the main thread is blocked in a
//! read that only the handler can break. So the handler here is fixed and
//! does only async-signal-safe things — `shutdown(2)`, `tcsetattr(3)`,
//! `_exit(2)` and atomics — and the program steers it by saying, ahead of
//! time, which connection a Ctrl-C would sever.

use crate::term::Termios;
use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::os::fd::RawFd;

/// The descriptor a Ctrl-C shuts down; -1 means Ctrl-C exits instead.
static SEVER: AtomicI32 = AtomicI32::new(-1);
/// Ctrl-C only records that it happened (a long job stops at its next step).
static JUST_NOTE: AtomicBool = AtomicBool::new(false);
/// A Ctrl-C was noted and nobody has asked about it yet.
static NOTED: AtomicBool = AtomicBool::new(false);
/// The exit code when Ctrl-C exits.
static EXIT_CODE: AtomicU8 = AtomicU8::new(1);
/// A connection was severed and nobody has asked about it yet.
static SEVERED: AtomicBool = AtomicBool::new(false);

/// The terminal mode to put back before exiting from the handler, so a
/// Ctrl-C that arrives as a signal while a terminal is raw does not leave the
/// shell unusable.
struct Restore {
    /// 0: nothing saved; 1: being written; 2: `fd` and `termios` are valid.
    state: AtomicU8,
    fd: AtomicI32,
    termios: UnsafeCell<core::mem::MaybeUninit<Termios>>,
}

// SAFETY: `termios` is written only by `remember_terminal` while `state` is
// 1, and read only by the handler when `state` is 2; a writer moves the
// state to 1 before writing and to 2 after, so a reader never sees a partial
// write. Only the main thread writes (one `RawMode` at a time).
unsafe impl Sync for Restore {}

static RESTORE: Restore = Restore {
    state: AtomicU8::new(0),
    fd: AtomicI32::new(-1),
    termios: UnsafeCell::new(core::mem::MaybeUninit::uninit()),
};

/// Install the Ctrl-C handler: from now on SIGINT exits with `exit_code`
/// unless [`sever_on_interrupt`] names a connection.
///
/// # Examples
///
/// ```
/// kevy_sys::install_interrupt(1);
/// kevy_sys::sever_on_interrupt(None);
/// assert!(!kevy_sys::take_severed());
/// ```
pub fn install_interrupt(exit_code: u8) {
    EXIT_CODE.store(exit_code, Ordering::Relaxed);
    crate::signal::install_signal_handler(crate::signal::SIGINT, on_interrupt);
}

/// Which connection a Ctrl-C cuts: `Some(fd)` shuts that socket down in both
/// directions (a blocked read on it returns) and records that it happened;
/// `None` makes Ctrl-C exit.
///
/// # Examples
///
/// ```
/// let (a, _b) = std::os::unix::net::UnixStream::pair()?;
/// use std::os::fd::AsRawFd;
/// kevy_sys::sever_on_interrupt(Some(a.as_raw_fd()));
/// kevy_sys::sever_on_interrupt(None);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn sever_on_interrupt(fd: Option<RawFd>) {
    SEVER.store(fd.unwrap_or(-1), Ordering::Relaxed);
}

/// Whether a Ctrl-C severed a connection since the last call.
///
/// # Examples
///
/// ```
/// assert!(!kevy_sys::take_severed());
/// ```
pub fn take_severed() -> bool {
    SEVERED.swap(false, Ordering::Relaxed)
}

/// Make Ctrl-C only note that it happened, for a job that stops cleanly at
/// its next step and reports what it has; see [`take_noted`]. This overrides
/// both exiting and severing until the process ends.
///
/// # Examples
///
/// ```
/// kevy_sys::install_interrupt(1);
/// kevy_sys::note_interrupts();
/// assert!(!kevy_sys::take_noted());
/// ```
pub fn note_interrupts() {
    JUST_NOTE.store(true, Ordering::Relaxed);
}

/// Whether a Ctrl-C was noted since the last call.
///
/// # Examples
///
/// ```
/// assert!(!kevy_sys::take_noted());
/// ```
pub fn take_noted() -> bool {
    NOTED.swap(false, Ordering::Relaxed)
}

/// Record the mode to restore on `fd` if the handler exits; `None` forgets it.
pub(crate) fn remember_terminal(saved: Option<(RawFd, &Termios)>) {
    RESTORE.state.store(1, Ordering::SeqCst);
    if let Some((fd, termios)) = saved {
        RESTORE.fd.store(fd, Ordering::SeqCst);
        // SAFETY: state is 1, so the handler does not read the cell; this is
        // the only writer (see `Restore`).
        unsafe { (*RESTORE.termios.get()).write(*termios) };
        RESTORE.state.store(2, Ordering::SeqCst);
    } else {
        RESTORE.state.store(0, Ordering::SeqCst);
    }
}

extern "C" fn on_interrupt(_signum: c_int) {
    if JUST_NOTE.load(Ordering::Relaxed) {
        NOTED.store(true, Ordering::Relaxed);
        return;
    }
    let fd = SEVER.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: shutdown(2) is async-signal-safe and takes plain integers;
        // on a descriptor that is not a socket it fails with ENOTSOCK and
        // changes nothing.
        unsafe { crate::ffi::shutdown(fd, SHUT_RDWR) };
        SEVERED.store(true, Ordering::Relaxed);
        return;
    }
    if RESTORE.state.load(Ordering::SeqCst) == 2 {
        // SAFETY: state 2 means the cell holds a complete `Termios` written
        // by `remember_terminal`; tcsetattr is async-signal-safe and only
        // reads it.
        unsafe {
            let saved = (*RESTORE.termios.get()).assume_init_ref();
            crate::ffi::tcsetattr(RESTORE.fd.load(Ordering::SeqCst), crate::term::TCSANOW, saved);
        }
    }
    // SAFETY: _exit(2) is async-signal-safe and never returns.
    unsafe { crate::ffi::_exit(c_int::from(EXIT_CODE.load(Ordering::Relaxed))) }
}

/// `SHUT_RDWR`: 2 on Linux and macOS.
const SHUT_RDWR: c_int = 2;

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    /// The handler, run for real by a SIGINT this process sends itself: the
    /// named socket is shut down (a read on it ends) and the event is
    /// reported once.
    #[test]
    fn a_ctrl_c_severs_the_named_connection_and_says_so_once() {
        let (mut ours, _theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        super::install_interrupt(1);
        super::sever_on_interrupt(Some(ours.as_raw_fd()));
        let pid = std::process::id().to_string();
        let sent = std::process::Command::new("kill").args(["-INT", &pid]).status();
        assert!(sent.is_ok_and(|s| s.success()), "kill -INT self");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !super::take_severed() {
            assert!(std::time::Instant::now() < deadline, "the handler never ran");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!super::take_severed(), "reported once");
        let mut buf = [0u8; 8];
        assert_eq!(ours.read(&mut buf).expect("a shut-down socket reads end-of-stream"), 0);
    }
}
