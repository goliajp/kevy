//! kevy-sys — kevy's network-boundary layer.
//!
//! One of kevy's three OS-boundary crates (alongside the publishable
//! [`kevy-uring`](https://crates.io/crates/kevy-uring) and
//! [`kevy-madvise`](https://crates.io/crates/kevy-madvise)). This is the
//! server-internal piece — hand-curated to the exact subset of sockets and the
//! readiness poller (kqueue on macOS, epoll on Linux) that kevy's server
//! needs. Every binding is declared by hand with `unsafe extern "C"`
//! (no `libc` crate, no third-party dep). On Linux these symbols resolve
//! through glibc, on macOS through libSystem — both already linked by
//! `std`, so we add zero dependencies.
//!
//! The poller here is *readiness*-based. The *completion*-based io_uring
//! engine has moved to its own crate, [`kevy-uring`]; either can back
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

pub(crate) mod ffi;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod poller_kq;
#[cfg(target_os = "linux")]
mod poller_ep;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use poller_kq::Poller;
#[cfg(target_os = "linux")]
pub use poller_ep::Poller;

// ---- constants -------------------------------------------------------------

const AF_INET: c_int = 2;
const AF_UNIX: c_int = 1;
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

// Field names mirror BSD's `<netinet/in.h>` struct sockaddr_in — the `sin_*`
// prefix is the ABI; renaming would just obscure the libc binding.
#[allow(clippy::struct_field_names)]
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
            let n = unsafe { ffi::read(self.fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
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
            let n = unsafe { ffi::write(self.fd, buf.as_ptr().cast::<c_void>(), buf.len()) };
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
                (&raw const one).cast::<c_void>(),
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
                (&raw mut addr).cast::<c_void>(),
                &raw mut len,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u16::from_be(addr.sin_port))
    }

    /// The peer's IPv4 address + port. Used by the replication
    /// listener to record the (ip, port) of every connected replica
    /// for `INFO replication` / `ROLE` reporting. IPv4 only —
    /// matches the rest of kevy-sys, which has not yet grown IPv6.
    pub fn peer_addr(&self) -> io::Result<(std::net::Ipv4Addr, u16)> {
        let mut addr = SockaddrIn::zeroed();
        let mut len = size_of::<SockaddrIn>() as u32;
        let r = unsafe {
            ffi::getpeername(
                self.fd,
                (&raw mut addr).cast::<c_void>(),
                &raw mut len,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        // `sin_addr` is stored in network byte order as a u32 of the
        // packed octets — `u32::from_ne_bytes(ip)` on construction
        // (see [`SockaddrIn::new_v4`]) is the inverse of the bytes
        // we want here.
        let octets = addr.sin_addr.to_ne_bytes();
        Ok((std::net::Ipv4Addr::from(octets), u16::from_be(addr.sin_port)))
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
            (&raw const val).cast::<c_void>(),
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
            (&raw const addr).cast::<c_void>(),
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

// ---- sockaddr_un (AF_UNIX) -------------------------------------------------

/// Unix-domain `sockaddr_un`. sun_path is 108 bytes on Linux + macOS BSD.
#[repr(C)]
struct SockaddrUn {
    #[cfg(target_os = "linux")]
    sun_family: u16,
    #[cfg(not(target_os = "linux"))]
    sun_len: u8,
    #[cfg(not(target_os = "linux"))]
    sun_family: u8,
    sun_path: [u8; 108],
}

impl SockaddrUn {
    fn new(path: &[u8]) -> io::Result<(Self, u32)> {
        if path.is_empty() || path.len() >= 108 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unix socket path must be 1..=107 bytes",
            ));
        }
        let mut sun_path = [0u8; 108];
        sun_path[..path.len()].copy_from_slice(path);
        // The actual length passed to bind() is offset_of(sun_path) + strlen(path) + 1
        // (for the NUL); using full struct size also works on Linux + BSD.
        let sa = SockaddrUn {
            #[cfg(target_os = "linux")]
            sun_family: AF_UNIX as u16,
            #[cfg(not(target_os = "linux"))]
            sun_len: size_of::<SockaddrUn>() as u8,
            #[cfg(not(target_os = "linux"))]
            sun_family: AF_UNIX as u8,
            sun_path,
        };
        Ok((sa, size_of::<SockaddrUn>() as u32))
    }
}

/// Create a blocking AF_UNIX stream listener bound to `path`. Unlinks any
/// existing file at the path first (mirroring valkey/redis's `unixsocket`
/// option). UDS bypasses the TCP stack — useful when client+server are on
/// the same host and the TCP loopback round-trip is the bench-shape floor.
pub fn unix_listen(path: &[u8], backlog: i32) -> io::Result<Socket> {
    // Best-effort unlink so subsequent bind doesn't EADDRINUSE on restart.
    // Convert path to a NUL-terminated CString for libc::unlink.
    if let Ok(c) = std::ffi::CString::new(path) {
        unsafe {
            ffi::unlink(c.as_ptr());
        }
    }

    let fd = unsafe { ffi::socket(AF_UNIX, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let sock = Socket { fd };

    let (addr, len) = SockaddrUn::new(path)?;
    let r = unsafe {
        ffi::bind(
            fd,
            (&raw const addr).cast::<c_void>(),
            len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { ffi::listen(fd, backlog) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // World-writable so clients with different uid can connect (redis SOP).
    // Use libc::chmod via CString.
    if let Ok(c) = std::ffi::CString::new(path) {
        unsafe { ffi::chmod(c.as_ptr(), 0o777) };
    }
    Ok(sock)
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
            let n = unsafe { ffi::read(self.read_fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
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



#[cfg(test)]
mod tests;
