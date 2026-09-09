//! macOS / iOS kqueue-backed `Poller`. Wired by `lib.rs` via a cfg-gated
//! `pub use`. The Linux epoll twin lives in [`crate::poller_ep`].

use core::ffi::c_int;
use std::io;
use std::ptr;

use crate::{Event, WAIT_CAPACITY, ffi};

mod kq {
    pub const EVFILT_READ: i16 = -1;
    pub const EVFILT_WRITE: i16 = -2;
    pub const EV_ADD: u16 = 0x0001;
    pub const EV_DELETE: u16 = 0x0002;
    pub const EV_ENABLE: u16 = 0x0004;
    pub const EV_DISABLE: u16 = 0x0008;
    pub const EV_EOF: u16 = 0x8000;
}

/// Edge/level-readiness poller. macOS: kqueue. Linux: epoll. Same API on both.
#[derive(Debug)]
pub struct Poller {
    kq: c_int,
}

impl Poller {
    /// Creates a fresh kqueue instance (closed on drop). Errors surface the
    /// raw OS error from `kqueue(2)`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `kqueue(2)` takes no arguments and dereferences nothing. A negative
        // return is checked below before the descriptor is wrapped.
        let kq = unsafe { ffi::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller { kq })
    }

    fn change(&self, fd: i32, filter: i16, flags: u16) -> io::Result<()> {
        let kev = ffi::Kevent { ident: fd as usize, filter, flags, fflags: 0, data: 0, udata: 0 };
        // SAFETY: `self.kq` is open for the life of this `Poller` — `Drop` is the only
        // close. `kev` is a live local and the changelist length passed is 1, matching it;
        // the eventlist is null with length 0, so nothing is written back.
        let r = unsafe { ffi::kevent(self.kq, &raw const kev, 1, ptr::null_mut(), 0, ptr::null()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Register `fd`, enabling the read/write filters per the interest flags.
    pub fn add(&self, fd: i32, read: bool, write: bool) -> io::Result<()> {
        let r = if read { kq::EV_ENABLE } else { kq::EV_DISABLE };
        let w = if write { kq::EV_ENABLE } else { kq::EV_DISABLE };
        self.change(fd, kq::EVFILT_READ, kq::EV_ADD | r)?;
        self.change(fd, kq::EVFILT_WRITE, kq::EV_ADD | w)?;
        Ok(())
    }

    /// Change the read/write interest of an already-registered `fd`.
    pub fn modify(&self, fd: i32, read: bool, write: bool) -> io::Result<()> {
        self.change(fd, kq::EVFILT_READ, if read { kq::EV_ENABLE } else { kq::EV_DISABLE })?;
        self.change(fd, kq::EVFILT_WRITE, if write { kq::EV_ENABLE } else { kq::EV_DISABLE })?;
        Ok(())
    }

    /// Best-effort deregistration of both filters.
    ///
    /// A filter kqueue has already dropped — because the fd closed, or
    /// because it was never enabled — answers `ENOENT`, which is the
    /// state being asked for. And both must be attempted: returning on
    /// the first leaves the write filter registered against an fd that
    /// is going away.
    #[expect(clippy::let_underscore_must_use, reason = "ENOENT here is the outcome wanted")]
    pub fn delete(&self, fd: i32) -> io::Result<()> {
        let _ = self.change(fd, kq::EVFILT_READ, kq::EV_DELETE);
        let _ = self.change(fd, kq::EVFILT_WRITE, kq::EV_DELETE);
        Ok(())
    }

    /// Wait for readiness, filling `out`. `timeout_ms == None` blocks forever.
    ///
    /// The kernel's event array is a stack array, not a `Vec`. This runs
    /// on every iteration of the shard's busy-poll body, and a
    /// `Vec::with_capacity` here was one malloc plus one free of 32 KB
    /// per iteration — a per-iteration cost, which is the category that
    /// has actually moved throughput in this project, unlike per-op
    /// microseconds. No measurement is claimed for it; what is claimed is
    /// that the allocation is gone and the signature did not move.
    ///
    /// `MaybeUninit` rather than a zeroed array: the kernel fills the
    /// first `n` entries and nothing reads past them, so zeroing 32 KB to
    /// satisfy the type system would cost more than what was removed.
    pub fn wait(&self, out: &mut Vec<Event>, timeout_ms: Option<i32>) -> io::Result<usize> {
        out.clear();
        let mut raw = [const { core::mem::MaybeUninit::<ffi::Kevent>::uninit() }; WAIT_CAPACITY];
        let ts = timeout_ms.map(|ms| ffi::Timespec {
            tv_sec: (ms / 1000) as isize,
            tv_nsec: ((ms % 1000) * 1_000_000) as isize,
        });
        // `None` means block forever, which `kevent(2)` spells as a null
        // timeout pointer. Borrowed from `ts`, which outlives the call.
        let ts_ptr = ts.as_ref().map_or(ptr::null(), |t| &raw const *t);
        // SAFETY: `self.kq` is open for the life of this `Poller`. The changelist is null
        // with length 0. `raw` is `WAIT_CAPACITY` elements and that same number is passed
        // as the eventlist length, so the kernel writes only within it. `ts_ptr` is either
        // null or points at `ts`, which outlives the call.
        let n = unsafe {
            ffi::kevent(
                self.kq,
                ptr::null(),
                0,
                raw.as_mut_ptr().cast::<ffi::Kevent>(),
                WAIT_CAPACITY as c_int,
                ts_ptr,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        // `n >= 0` was checked above and `n <= WAIT_CAPACITY` is the eventlist length we
        // passed, so this slice is inside the array and every element of it was written
        // by the kernel.
        for kev in &raw[..n as usize] {
            // SAFETY: `kevent(2)` reported `n` events, so each element of this slice was
            // initialised by the kernel before it returned.
            out.push(translate(unsafe { kev.assume_init_ref() }));
        }
        Ok(out.len())
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        // SAFETY: `self.kq` was open for the life of this `Poller` and this is the only
        // close: `Poller` is neither `Copy` nor `Clone`, so no second owner can close it
        // again.
        unsafe {
            ffi::close(self.kq);
        }
    }
}

/// One kernel `kevent` as a poller [`Event`].
fn translate(kev: &ffi::Kevent) -> Event {
    Event {
        fd: kev.ident as i32,
        readable: kev.filter == kq::EVFILT_READ,
        writable: kev.filter == kq::EVFILT_WRITE,
        hup: kev.flags & kq::EV_EOF != 0,
    }
}
