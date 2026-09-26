//! OS entropy: `getrandom(2)` on Linux and Android, `getentropy(3)` on
//! macOS and iOS. Both block until the kernel pool is seeded and never
//! return fewer bytes than asked without an error, apart from the partial
//! reads `getrandom` may make and this loop resumes.

use core::ffi::c_void;
use std::io;

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe extern "C" {
    fn getrandom(buf: *mut c_void, buflen: usize, flags: u32) -> isize;
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
unsafe extern "C" {
    fn getentropy(buf: *mut c_void, buflen: usize) -> core::ffi::c_int;
}

/// Fill `buf` with bytes from the operating system's CSPRNG.
///
/// ```
/// let mut a = [0u8; 32];
/// let mut b = [0u8; 32];
/// kevy_sys::fill_random(&mut a).unwrap();
/// kevy_sys::fill_random(&mut b).unwrap();
/// assert_ne!(a, b);
/// ```
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let rest = &mut buf[done..];
        // SAFETY: `rest` is a live, writable slice and the length passed is
        // its own; getrandom writes at most that many bytes into it.
        let n = unsafe { getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        done += n as usize;
    }
    Ok(())
}

/// Fill `buf` with bytes from the operating system's CSPRNG.
///
/// ```
/// let mut a = [0u8; 32];
/// let mut b = [0u8; 32];
/// kevy_sys::fill_random(&mut a).unwrap();
/// kevy_sys::fill_random(&mut b).unwrap();
/// assert_ne!(a, b);
/// ```
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    // getentropy refuses requests above 256 bytes
    for chunk in buf.chunks_mut(256) {
        // SAFETY: `chunk` is a live, writable slice of at most 256 bytes and
        // the length passed is its own.
        if unsafe { getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
