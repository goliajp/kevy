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
pub struct Poller {
    kq: c_int,
}

impl Poller {
    pub fn new() -> io::Result<Self> {
        let kq = unsafe { ffi::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller { kq })
    }

    fn change(&self, fd: i32, filter: i16, flags: u16) -> io::Result<()> {
        let kev = ffi::Kevent {
            ident: fd as usize,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: 0,
        };
        let r = unsafe { ffi::kevent(self.kq, &kev, 1, ptr::null_mut(), 0, ptr::null()) };
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
        self.change(
            fd,
            kq::EVFILT_READ,
            if read { kq::EV_ENABLE } else { kq::EV_DISABLE },
        )?;
        self.change(
            fd,
            kq::EVFILT_WRITE,
            if write { kq::EV_ENABLE } else { kq::EV_DISABLE },
        )?;
        Ok(())
    }

    /// Best-effort deregistration of both filters.
    pub fn delete(&self, fd: i32) -> io::Result<()> {
        let _ = self.change(fd, kq::EVFILT_READ, kq::EV_DELETE);
        let _ = self.change(fd, kq::EVFILT_WRITE, kq::EV_DELETE);
        Ok(())
    }

    /// Wait for readiness, filling `out`. `timeout_ms == None` blocks forever.
    pub fn wait(&self, out: &mut Vec<Event>, timeout_ms: Option<i32>) -> io::Result<usize> {
        out.clear();
        let mut raw: Vec<ffi::Kevent> = Vec::with_capacity(WAIT_CAPACITY);
        let ts;
        let ts_ptr = match timeout_ms {
            Some(ms) => {
                ts = ffi::Timespec {
                    tv_sec: (ms / 1000) as isize,
                    tv_nsec: ((ms % 1000) * 1_000_000) as isize,
                };
                &ts as *const ffi::Timespec
            }
            None => ptr::null(),
        };
        let n = unsafe {
            ffi::kevent(
                self.kq,
                ptr::null(),
                0,
                raw.as_mut_ptr(),
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
        unsafe { raw.set_len(n as usize) };
        for kev in &raw {
            out.push(Event {
                fd: kev.ident as i32,
                readable: kev.filter == kq::EVFILT_READ,
                writable: kev.filter == kq::EVFILT_WRITE,
                hup: kev.flags & kq::EV_EOF != 0,
            });
        }
        Ok(out.len())
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        unsafe {
            ffi::close(self.kq);
        }
    }
}
