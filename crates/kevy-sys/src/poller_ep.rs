//! Linux epoll-backed `Poller`. Wired by `lib.rs` via a cfg-gated
//! `pub use`. The macOS / iOS kqueue twin lives in [`crate::poller_kq`].

use core::ffi::c_int;
use std::io;
use std::ptr;

use crate::{Event, WAIT_CAPACITY, ffi};

mod ep {
    pub const EPOLL_CLOEXEC: super::c_int = 0x80000;
    pub const EPOLL_CTL_ADD: super::c_int = 1;
    pub const EPOLL_CTL_DEL: super::c_int = 2;
    pub const EPOLL_CTL_MOD: super::c_int = 3;
    pub const EPOLLIN: u32 = 0x001;
    pub const EPOLLOUT: u32 = 0x004;
    pub const EPOLLERR: u32 = 0x008;
    pub const EPOLLHUP: u32 = 0x010;
    pub const EPOLLRDHUP: u32 = 0x2000;
}

pub struct Poller {
    epfd: c_int,
}

impl Poller {
    pub fn new() -> io::Result<Self> {
        let epfd = unsafe { ffi::epoll_create1(ep::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Poller { epfd })
    }

    fn mask(read: bool, write: bool) -> u32 {
        let mut m = ep::EPOLLRDHUP;
        if read {
            m |= ep::EPOLLIN;
        }
        if write {
            m |= ep::EPOLLOUT;
        }
        m
    }

    fn ctl(&self, op: c_int, fd: i32, read: bool, write: bool) -> io::Result<()> {
        let mut ev = ffi::EpollEvent {
            events: Self::mask(read, write),
            data: fd as u64,
        };
        let r = unsafe { ffi::epoll_ctl(self.epfd, op, fd, &mut ev) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn add(&self, fd: i32, read: bool, write: bool) -> io::Result<()> {
        self.ctl(ep::EPOLL_CTL_ADD, fd, read, write)
    }

    pub fn modify(&self, fd: i32, read: bool, write: bool) -> io::Result<()> {
        self.ctl(ep::EPOLL_CTL_MOD, fd, read, write)
    }

    pub fn delete(&self, fd: i32) -> io::Result<()> {
        let r = unsafe { ffi::epoll_ctl(self.epfd, ep::EPOLL_CTL_DEL, fd, ptr::null_mut()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn wait(&self, out: &mut Vec<Event>, timeout_ms: Option<i32>) -> io::Result<usize> {
        out.clear();
        let mut raw: Vec<ffi::EpollEvent> = Vec::with_capacity(WAIT_CAPACITY);
        let n = unsafe {
            ffi::epoll_wait(
                self.epfd,
                raw.as_mut_ptr(),
                WAIT_CAPACITY as c_int,
                timeout_ms.unwrap_or(-1),
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
        for ev in &raw {
            let flags = ev.events; // copy out (struct may be packed on x86_64)
            let fd = ev.data as i32;
            let hup = flags & (ep::EPOLLHUP | ep::EPOLLERR | ep::EPOLLRDHUP) != 0;
            out.push(Event {
                fd,
                readable: flags & (ep::EPOLLIN | ep::EPOLLHUP | ep::EPOLLERR) != 0,
                writable: flags & ep::EPOLLOUT != 0,
                hup,
            });
        }
        Ok(out.len())
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        unsafe {
            ffi::close(self.epfd);
        }
    }
}
