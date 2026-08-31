//! `flock(2)` — the advisory whole-file lock behind kevy-persist's
//! directory claim (`DirLock`): one engine per data dir, enforced by
//! the kernel rather than by a pid heuristic. The lock belongs to the
//! open file description, so dropping the `File` — or the process
//! dying, however hard — releases it; nothing can go stale.

use crate::ffi;
use core::ffi::c_int;
use std::io;

const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;

/// Try to take the exclusive advisory lock on `fd` without blocking.
///
/// `WouldBlock` means another open file description already holds it —
/// in `flock` terms another process, or another `File` opened on the
/// same path inside this one (each `open` is its own description, so
/// the same-process double-open is caught too).
///
/// ```
/// use std::os::fd::AsRawFd;
/// let path = std::env::temp_dir().join(format!("flock-doc-{}", std::process::id()));
/// let f = std::fs::File::create(&path)?;
/// kevy_sys::flock_try_exclusive(f.as_raw_fd())?; // first claim: ours
/// # std::fs::remove_file(&path).ok();
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn flock_try_exclusive(fd: i32) -> io::Result<()> {
    // SAFETY: flock(2) on a fd the caller owns; no pointers cross.
    let rc = unsafe { ffi::flock(fd, LOCK_EX | LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
