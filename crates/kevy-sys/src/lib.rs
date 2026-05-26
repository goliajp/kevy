//! kevy-sys — kevy's network-boundary layer.
//!
//! One of kevy's three OS-boundary crates (alongside the publishable stones
//! [`kevy-uring`](https://crates.io/crates/kevy-uring) and
//! [`kevy-madvise`](https://crates.io/crates/kevy-madvise)). This is the
//! cement piece — hand-curated to the exact subset of sockets and the
//! readiness poller (kqueue on macOS, epoll on Linux) that kevy's server
//! needs. Every binding is declared by hand with `unsafe extern "C"`
//! (no `libc` crate, no third-party dep). On Linux these symbols resolve
//! through glibc, on macOS through libSystem — both already linked by
//! `std`, so we add zero dependencies.
//!
//! The poller here is *readiness*-based. The *completion*-based io_uring
//! engine has moved to its own stone crate, [`kevy-uring`]; either can back
//! the reactor on top ([kevy-net]), which exposes only a byte-level
//! service contract.
//!
//! Part of the [kevy] key–value server; not a generic OS-binding library.
//!
//! [`kevy-uring`]: https://crates.io/crates/kevy-uring
//!
//! # Safety
//!
//! `unsafe` is confined to the private `ffi` module's `extern "C"` declarations
//! and the thin wrappers that call them. The bindings match the platform libc
//! ABI (socklen_t = `u32`; `struct sockaddr_in`, `kevent`, and `epoll_event`
//! laid out per-OS/arch). All raw fds are owned by RAII types ([`Socket`],
//! [`Poller`], [`Waker`]) that close on drop, and errors are read via
//! `std::io::Error::last_os_error()`. The public API is safe.
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-net]: https://crates.io/crates/kevy-net
//!
//! # Example
//!
//! ```no_run
//! use kevy_sys::{Poller, tcp_listen};
//!
//! # fn main() -> std::io::Result<()> {
//! let listener = tcp_listen([127, 0, 0, 1], 6379, 1024)?;
//! listener.set_nonblocking()?;
//!
//! let poller = Poller::new()?;
//! poller.add(listener.raw(), /* read */ true, /* write */ false)?;
//!
//! let mut events = Vec::new();
//! poller.wait(&mut events, Some(1000))?; // block up to 1s
//! for ev in &events {
//!     if ev.fd == listener.raw() && ev.readable {
//!         let conn = listener.accept()?;
//!         conn.set_nodelay()?;
//!         // ... read/write `conn` ...
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use core::ffi::{c_int, c_void};
use core::mem::size_of;
use core::ptr;
use std::io;

mod ffi {
    use core::ffi::{c_int, c_void};

    // socklen_t is u32 on both Linux and macOS.
    unsafe extern "C" {
        pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn setsockopt(
            fd: c_int,
            level: c_int,
            optname: c_int,
            optval: *const c_void,
            optlen: u32,
        ) -> c_int;
        pub fn bind(fd: c_int, addr: *const c_void, addrlen: u32) -> c_int;
        pub fn listen(fd: c_int, backlog: c_int) -> c_int;
        pub fn accept(fd: c_int, addr: *mut c_void, addrlen: *mut u32) -> c_int;
        pub fn getsockname(fd: c_int, addr: *mut c_void, addrlen: *mut u32) -> c_int;
        pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        pub fn close(fd: c_int) -> c_int;
        // Variadic in C; we only ever pass a single int arg (F_GETFL/F_SETFL).
        pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        pub fn pipe(fds: *mut c_int) -> c_int;
    }

    /// `struct timespec` — used by kqueue's `kevent` timeout (macOS only).
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[repr(C)]
    pub struct Timespec {
        pub tv_sec: isize,
        pub tv_nsec: isize,
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[repr(C)]
    pub struct Kevent {
        pub ident: usize,
        pub filter: i16,
        pub flags: u16,
        pub fflags: u32,
        pub data: isize,
        pub udata: usize, // really `void*`; we never use it, keep it integral (Send)
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    unsafe extern "C" {
        pub fn kqueue() -> c_int;
        pub fn kevent(
            kq: c_int,
            changelist: *const Kevent,
            nchanges: c_int,
            eventlist: *mut Kevent,
            nevents: c_int,
            timeout: *const Timespec,
        ) -> c_int;
    }

    // `struct epoll_event` is `__attribute__((packed))` only on x86_64; on every
    // other arch it is naturally aligned (8-byte `data` after 4-byte `events`,
    // with 4 bytes of padding). Match the kernel ABI exactly.
    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[cfg_attr(target_arch = "x86_64", repr(packed))]
    pub struct EpollEvent {
        pub events: u32,
        pub data: u64,
    }

    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        pub fn epoll_create1(flags: c_int) -> c_int;
        pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
        pub fn epoll_wait(
            epfd: c_int,
            events: *mut EpollEvent,
            maxevents: c_int,
            timeout: c_int,
        ) -> c_int;
    }

}

// ---- constants -------------------------------------------------------------

const AF_INET: c_int = 2;
const SOCK_STREAM: c_int = 1;
const IPPROTO_TCP: c_int = 6;
const TCP_NODELAY: c_int = 1;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;

#[cfg(target_os = "linux")]
const SOL_SOCKET: c_int = 1;
#[cfg(target_os = "linux")]
const SO_REUSEADDR: c_int = 2;
#[cfg(target_os = "linux")]
const SO_REUSEPORT: c_int = 15;
#[cfg(target_os = "linux")]
const O_NONBLOCK: c_int = 0x800;

#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOL_SOCKET: c_int = 0xffff;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SO_REUSEADDR: c_int = 0x0004;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SO_REUSEPORT: c_int = 0x0200;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const O_NONBLOCK: c_int = 0x0004;

// ---- sockaddr_in -----------------------------------------------------------

#[cfg(target_os = "linux")]
#[repr(C)]
struct SockaddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
struct SockaddrIn {
    sin_len: u8,
    sin_family: u8,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

impl SockaddrIn {
    fn new(ip: [u8; 4], port: u16) -> Self {
        #[cfg(target_os = "linux")]
        return SockaddrIn {
            sin_family: AF_INET as u16,
            sin_port: port.to_be(),
            sin_addr: u32::from_ne_bytes(ip),
            sin_zero: [0; 8],
        };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        return SockaddrIn {
            sin_len: size_of::<SockaddrIn>() as u8,
            sin_family: AF_INET as u8,
            sin_port: port.to_be(),
            sin_addr: u32::from_ne_bytes(ip),
            sin_zero: [0; 8],
        };
    }

    fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

// ---- Socket ----------------------------------------------------------------

/// An owned socket file descriptor. Closes itself on drop via our own `close`.
pub struct Socket {
    fd: c_int,
}

impl Socket {
    /// The raw file descriptor. Borrowed — the `Socket` retains ownership.
    #[inline]
    pub fn raw(&self) -> i32 {
        self.fd
    }

    /// Wrap an already-open fd (e.g. one accepted by io_uring) into an owning
    /// `Socket` that closes it on drop.
    ///
    /// # Safety
    /// `fd` must be a valid open descriptor whose ownership is transferred here.
    #[inline]
    pub unsafe fn from_raw_fd(fd: i32) -> Socket {
        Socket { fd }
    }

    /// Accept one inbound connection. On a non-blocking listener with no pending
    /// connection this returns `Err` with kind `WouldBlock`.
    pub fn accept(&self) -> io::Result<Socket> {
        let fd = unsafe { ffi::accept(self.fd, ptr::null_mut(), ptr::null_mut()) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Socket { fd })
    }

    /// Read into `buf`, returning the byte count (0 == EOF). Retries on EINTR.
    /// On a non-blocking socket with no data, returns `WouldBlock`.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { ffi::read(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(n as usize);
        }
    }

    /// A single `write` syscall; may write fewer bytes than requested, or return
    /// `WouldBlock` on a full non-blocking socket. Retries on EINTR.
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { ffi::write(self.fd, buf.as_ptr() as *const c_void, buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(n as usize);
        }
    }

    /// Write the whole buffer (blocking-socket convenience).
    pub fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.write(buf)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
            }
            buf = &buf[n..];
        }
        Ok(())
    }

    /// Put the socket into non-blocking mode (`O_NONBLOCK`).
    pub fn set_nonblocking(&self) -> io::Result<()> {
        set_fd_nonblocking(self.fd)
    }

    /// Disable Nagle's algorithm (`TCP_NODELAY`) for low-latency replies.
    pub fn set_nodelay(&self) -> io::Result<()> {
        let one: c_int = 1;
        let r = unsafe {
            ffi::setsockopt(
                self.fd,
                IPPROTO_TCP,
                TCP_NODELAY,
                &one as *const c_int as *const c_void,
                size_of::<c_int>() as u32,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The local port this socket is bound to (host byte order).
    pub fn local_port(&self) -> io::Result<u16> {
        let mut addr = SockaddrIn::zeroed();
        let mut len = size_of::<SockaddrIn>() as u32;
        let r = unsafe {
            ffi::getsockname(
                self.fd,
                &mut addr as *mut SockaddrIn as *mut c_void,
                &mut len,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u16::from_be(addr.sin_port))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe {
            ffi::close(self.fd);
        }
    }
}

/// Set `O_NONBLOCK` on a raw fd (sockets and pipe ends alike).
fn set_fd_nonblocking(fd: c_int) -> io::Result<()> {
    let flags = unsafe { ffi::fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { ffi::fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn setsockopt_int(fd: c_int, level: c_int, name: c_int, val: c_int) -> io::Result<()> {
    let r = unsafe {
        ffi::setsockopt(
            fd,
            level,
            name,
            &val as *const c_int as *const c_void,
            size_of::<c_int>() as u32,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn listen_inner(ip: [u8; 4], port: u16, backlog: i32, reuseport: bool) -> io::Result<Socket> {
    let fd = unsafe { ffi::socket(AF_INET, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let sock = Socket { fd }; // owns fd: any early return closes it

    setsockopt_int(fd, SOL_SOCKET, SO_REUSEADDR, 1)?;
    if reuseport {
        // Each thread-per-core shard opens its own listener on the same port;
        // the kernel load-balances accepted connections across them.
        setsockopt_int(fd, SOL_SOCKET, SO_REUSEPORT, 1)?;
    }

    let addr = SockaddrIn::new(ip, port);
    let r = unsafe {
        ffi::bind(
            fd,
            &addr as *const SockaddrIn as *const c_void,
            size_of::<SockaddrIn>() as u32,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { ffi::listen(fd, backlog) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// Create a blocking IPv4 TCP listener bound to `ip:port` with `SO_REUSEADDR`.
/// Pass `port == 0` to let the OS assign an ephemeral port.
pub fn tcp_listen(ip: [u8; 4], port: u16, backlog: i32) -> io::Result<Socket> {
    listen_inner(ip, port, backlog, false)
}

/// Like [`tcp_listen`] but also sets `SO_REUSEPORT`, so multiple listeners can
/// share one port (one per thread-per-core shard).
pub fn tcp_listen_reuseport(ip: [u8; 4], port: u16, backlog: i32) -> io::Result<Socket> {
    listen_inner(ip, port, backlog, true)
}

/// A self-pipe used to wake a [`Poller`] blocked in `wait` from another thread.
/// Register `read_fd()` in the poller for read-readiness; call `wake()` from any
/// thread to make the poll return; call `drain()` when the read end fires.
pub struct Waker {
    read_fd: c_int,
    write_fd: c_int,
}

/// Create a non-blocking self-pipe waker.
pub fn waker() -> io::Result<Waker> {
    let mut fds = [0 as c_int; 2];
    if unsafe { ffi::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let w = Waker {
        read_fd: fds[0],
        write_fd: fds[1],
    };
    set_fd_nonblocking(w.read_fd)?;
    set_fd_nonblocking(w.write_fd)?;
    Ok(w)
}

impl Waker {
    /// The read end — register this in a [`Poller`] for read-readiness.
    #[inline]
    pub fn read_fd(&self) -> i32 {
        self.read_fd
    }

    /// Signal the waker. A full pipe already means "pending", so EAGAIN is fine.
    pub fn wake(&self) -> io::Result<()> {
        let byte = [1u8];
        loop {
            let n = unsafe { ffi::write(self.write_fd, byte.as_ptr() as *const c_void, 1) };
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
            let n = unsafe { ffi::read(self.read_fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n <= 0 {
                break; // EAGAIN / EOF / error — nothing more to drain
            }
        }
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        unsafe {
            ffi::close(self.read_fd);
            ffi::close(self.write_fd);
        }
    }
}

// The pipe ends are plain fds with no aliasing; safe to move across threads.
unsafe impl Send for Waker {}
unsafe impl Sync for Waker {}

// ---- Poller ----------------------------------------------------------------

/// A readiness notification for one file descriptor.
#[derive(Debug, Clone, Copy)]
pub struct Event {
    pub fd: i32,
    pub readable: bool,
    pub writable: bool,
    /// Peer hang-up / error — the connection should be closed.
    pub hup: bool,
}

/// How many raw events to pull from the kernel per `wait` call.
const WAIT_CAPACITY: usize = 1024;

#[cfg(any(target_os = "macos", target_os = "ios"))]
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
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub struct Poller {
    kq: c_int,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub struct Poller {
    epfd: c_int,
}

#[cfg(target_os = "linux")]
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

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
impl Drop for Poller {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        let fd = self.epfd;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let fd = self.kq;
        unsafe {
            ffi::close(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn listen_accept_roundtrip() {
        let listener = tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
        let port = listener.local_port().unwrap();
        assert_ne!(port, 0);

        let server = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            let mut b = [0u8; 1];
            assert_eq!(conn.read(&mut b).unwrap(), 1);
            conn.write_all(&b).unwrap();
        });

        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(b"Z").unwrap();
        let mut got = [0u8; 1];
        assert_eq!(client.read(&mut got).unwrap(), 1);
        assert_eq!(&got, b"Z");

        server.join().unwrap();
    }

    #[test]
    fn poller_signals_listener_readable() {
        let listener = tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
        listener.set_nonblocking().unwrap();
        let port = listener.local_port().unwrap();

        let poller = Poller::new().unwrap();
        poller.add(listener.raw(), true, false).unwrap();

        let _client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

        let mut events = Vec::new();
        let n = poller.wait(&mut events, Some(2000)).unwrap();
        assert!(n >= 1, "expected a readiness event");
        assert!(events.iter().any(|e| e.fd == listener.raw() && e.readable));

        // Non-blocking accept should now succeed.
        listener.accept().unwrap();
    }

    #[test]
    fn waker_wakes_poller() {
        let w = std::sync::Arc::new(waker().unwrap());
        let poller = Poller::new().unwrap();
        poller.add(w.read_fd(), true, false).unwrap();

        let w2 = w.clone();
        std::thread::spawn(move || w2.wake().unwrap());

        let mut events = Vec::new();
        let n = poller.wait(&mut events, Some(2000)).unwrap();
        assert!(n >= 1, "waker should have woken the poller");
        assert!(events.iter().any(|e| e.fd == w.read_fd() && e.readable));
        w.drain();
    }

    #[test]
    fn reuseport_allows_shared_port() {
        let l1 = tcp_listen_reuseport([127, 0, 0, 1], 0, 16).unwrap();
        let port = l1.local_port().unwrap();
        // A second listener on the SAME port succeeds only because of SO_REUSEPORT.
        let l2 = tcp_listen_reuseport([127, 0, 0, 1], port, 16).unwrap();
        assert_eq!(l2.local_port().unwrap(), port);
    }
}
