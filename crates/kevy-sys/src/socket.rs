//! The owned [`Socket`] RAII fd plus the TCP / AF_UNIX listener
//! constructors. Split out of `lib.rs` to keep it under the 500-LOC house
//! cap; every wrapper (and its unsafe block) is verbatim from before the
//! move — see the crate-level `# Safety` section.

use crate::addr::{
    AF_INET, AF_UNIX, F_GETFL, F_SETFL, IPPROTO_TCP, O_NONBLOCK, SO_REUSEADDR, SO_REUSEPORT,
    SOCK_STREAM, SOL_SOCKET, SockaddrIn, SockaddrUn, TCP_NODELAY,
};
use crate::ffi;
use core::ffi::{c_int, c_void};
use core::mem::size_of;
use core::ptr;
use std::io;

/// An owned socket file descriptor. Closes itself on drop via our own `close`.
pub struct Socket {
    pub(crate) fd: c_int,
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

    /// Whether the peer has gone — a non-blocking, non-consuming probe that
    /// asks the kernel directly rather than the reactor's completion backlog.
    ///
    /// `recv` with `MSG_PEEK | MSG_DONTWAIT` reads nothing from the stream:
    /// it returns 0 once the peer's FIN has reached the kernel (EOF), a
    /// positive count if unread bytes are queued (peer alive, and its FIN
    /// hasn't been read yet either way), or `EWOULDBLOCK` when the socket is
    /// open with nothing pending. So `true` = the connection is closed or
    /// reset even if this shard has not yet reaped the recv completion that
    /// would tell it so — which is exactly the case a cross-shard block
    /// serve must catch before delivering a popped element to a dead client.
    /// `EINTR` retries; any other error is treated as gone.
    pub fn peer_gone(&self) -> bool {
        let mut byte = [0u8; 1];
        loop {
            let n = unsafe {
                ffi::recv(
                    self.fd,
                    byte.as_mut_ptr().cast::<c_void>(),
                    1,
                    crate::addr::MSG_PEEK | crate::addr::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                return true; // EOF: peer sent FIN
            }
            if n > 0 {
                return false; // unread data queued: peer still there
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // WouldBlock = open, nothing pending = alive. Anything else
            // (ECONNRESET, ENOTCONN, EBADF, …) = gone.
            return e.kind() != io::ErrorKind::WouldBlock;
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
        let r =
            unsafe { ffi::getsockname(self.fd, (&raw mut addr).cast::<c_void>(), &raw mut len) };
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
        let r =
            unsafe { ffi::getpeername(self.fd, (&raw mut addr).cast::<c_void>(), &raw mut len) };
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
pub(crate) fn set_fd_nonblocking(fd: c_int) -> io::Result<()> {
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
        ffi::bind(fd, (&raw const addr).cast::<c_void>(), size_of::<SockaddrIn>() as u32)
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
    let r = unsafe { ffi::bind(fd, (&raw const addr).cast::<c_void>(), len) };
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
