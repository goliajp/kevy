//! A pseudo-terminal pair, for driving a program that needs a terminal from
//! a test: the program gets the child side as its stdin/stdout, the test
//! reads and writes the parent side.

use core::ffi::c_int;
use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Mutex;

/// `O_RDWR`, the same on both platforms.
const O_RDWR: c_int = 2;
/// `O_NOCTTY`: do not become the calling process's controlling terminal.
#[cfg(any(target_os = "linux", target_os = "android"))]
const O_NOCTTY: c_int = 0x100;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const O_NOCTTY: c_int = 0x2_0000;

/// `ptsname` returns a pointer into static storage; one caller at a time.
static PTSNAME: Mutex<()> = Mutex::new(());

/// Open a new pseudo-terminal. Returns the parent side, opened for reading
/// and writing, and the path of the child side, for a program to open.
///
/// # Errors
///
/// The error of `posix_openpt`, `grantpt`, `unlockpt` or `ptsname`, e.g.
/// when the system has run out of ptys.
///
/// # Examples
///
/// ```
/// use std::io::Write;
/// let (mut parent, child_path) = kevy_sys::open_pty()?;
/// let child = std::fs::OpenOptions::new().read(true).write(true).open(&child_path)?;
/// use std::os::fd::AsRawFd;
/// // A pty child is a terminal, so raw mode applies.
/// let raw = kevy_sys::RawMode::enable(child.as_raw_fd())?;
/// parent.write_all(b"x")?;
/// drop(raw);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn open_pty() -> io::Result<(File, PathBuf)> {
    // SAFETY: posix_openpt takes flags by value and returns a new descriptor
    // or -1; no memory is shared.
    let fd = unsafe { crate::ffi::posix_openpt(O_RDWR | O_NOCTTY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by posix_openpt and is owned by nothing
    // else, so the File may take ownership and close it on every error path.
    let parent = unsafe { File::from_raw_fd(fd) };
    // SAFETY: both take the descriptor by value and act on the pty behind it.
    if unsafe { crate::ffi::grantpt(fd) } != 0 || unsafe { crate::ffi::unlockpt(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _guard = PTSNAME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: ptsname returns NULL or a NUL-terminated string in static
    // storage, valid until the next ptsname call — which the mutex above
    // excludes until the bytes are copied out below.
    let name = unsafe { crate::ffi::ptsname(fd) };
    if name.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: non-null and NUL-terminated, per ptsname's contract above.
    let bytes = unsafe { core::ffi::CStr::from_ptr(name) }.to_bytes().to_vec();
    use std::os::unix::ffi::OsStringExt;
    Ok((parent, PathBuf::from(std::ffi::OsString::from_vec(bytes))))
}
