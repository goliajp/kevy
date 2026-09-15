//! The self-pipe [`Waker`] that interrupts a blocked `Poller::wait` from
//! another thread. Split out of `lib.rs` to keep it under the 500-LOC
//! house cap; verbatim from before the move.

use crate::ffi;
use crate::socket::set_fd_nonblocking;
use core::ffi::{c_int, c_void};
use std::io;

/// A self-pipe used to wake a [`Poller`](crate::Poller) blocked in `wait`
/// from another thread. Register `read_fd()` in the poller for
/// read-readiness; call `wake()` from any thread to make the poll return;
/// call `drain()` when the read end fires.
#[derive(Debug)]
pub struct Waker {
    read_fd: c_int,
    write_fd: c_int,
}

/// Create a non-blocking self-pipe waker.
pub fn waker() -> io::Result<Waker> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` is a live 2-element array on this frame, which is exactly what
    // `pipe(2)` writes into.
    if unsafe { ffi::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let w = Waker { read_fd: fds[0], write_fd: fds[1] };
    set_fd_nonblocking(w.read_fd)?;
    set_fd_nonblocking(w.write_fd)?;
    Ok(w)
}

impl Waker {
    /// The read end — register this in a [`Poller`](crate::Poller) for
    /// read-readiness.
    #[inline]
    pub fn read_fd(&self) -> i32 {
        self.read_fd
    }

    /// Signal the waker. A full pipe already means "pending", so EAGAIN is fine.
    pub fn wake(&self) -> io::Result<()> {
        let byte = [1u8];
        loop {
            // SAFETY: `self.write_fd` is open for the life of this `Waker` — `Drop` is the
            // only close. `byte` is a live 1-byte array and the length passed is 1.
            let n = unsafe { ffi::write(self.write_fd, byte.as_ptr().cast::<c_void>(), 1) };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => return Ok(()),
                    _ => return Err(e),
                }
            }
            return Ok(());
        }
    }

    /// Consume all pending wake bytes after the read end fires.
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            let n =
                // SAFETY: `self.read_fd` is open for the life of this `Waker`. `buf` is a live
                // mutable array, so its pointer is valid for `buf.len()` writes.
                unsafe { ffi::read(self.read_fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
            if n <= 0 {
                break; // EAGAIN / EOF / error — nothing more to drain
            }
        }
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        // SAFETY: both fds were open for the life of this `Waker` and this is the only
        // close: `Waker` is neither `Copy` nor `Clone`, so no second owner can close
        // them again.
        unsafe {
            ffi::close(self.read_fd);
            ffi::close(self.write_fd);
        }
    }
}

// `Waker` is `Send + Sync` by auto-derive, and deliberately says so by
// saying nothing: it holds two `c_int` fds and nothing else, and a
// descriptor is an integer handle into a kernel table rather than a
// pointer into this process.
//
// There were `unsafe impl Send` and `unsafe impl Sync` here. Both were
// no-ops — the auto impls already applied — but an explicit `unsafe
// impl` opts the type out of the compiler's check permanently. The day
// someone adds a `*mut c_void` or a `Cell` to this struct, say a cached
// buffer for `drain`, the compiler would have stayed silent about a type
// that had genuinely stopped being `Sync`. `Socket` and `Poller` have
// the same shape and carry no such impls, which is what made these two
// look defensive rather than required.
//
// The reasoning they carried is worth keeping: the only shared-reference
// operations are `wake` (a one-byte `write`) and `drain` (a `read`), the
// kernel serialises both, and a one-byte pipe write is atomic — so
// concurrent callers race only over which of them wakes the poller,
// which is the intended semantics.
//
// The requirement is still stated, and now the compiler is what checks
// it: if a field is added that is not `Send + Sync`, this fails to build
// here rather than at some distant call site — or, with the manual
// impls, not at all.
//
// A `where` clause rather than a closure calling a generic function: the
// closure form is never invoked, so it is a body that by construction
// cannot be executed — five never-executed regions handed to the
// coverage ratchet in exchange for a check a type definition does for
// free.
struct _WakerIsSendSync
where
    Waker: Send + Sync;
