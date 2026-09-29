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
//! use kevy_sys::{Interest, Poller, Socket};
//!
//! # fn main() -> std::io::Result<()> {
//! let listener = Socket::tcp_listen([127, 0, 0, 1], 6379, 1024)?;
//! listener.set_nonblocking()?;
//!
//! let poller = Poller::new()?;
//! poller.add(listener.raw(), Interest::READ)?;
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

#![warn(missing_docs)]
// Every `unsafe` block in this crate carries a `// SAFETY:` premise, and the
// lint keeps it that way. This is the OS boundary: the one place where a
// mistake is not caught by the type system, so the argument has to be written
// down where the call is, not inferred later from the call site.

pub(crate) mod addr;
pub mod checksum;
pub(crate) mod ffi;
mod interrupt;
mod lockfile;
mod map;
mod mem;
mod pty;
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
mod random;
mod signal;
mod socket;
mod term;
mod wait;
mod waker;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod poller_ep;
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod poller_kq;

pub use interrupt::{
    install_interrupt, note_interrupts, sever_on_interrupt, take_noted, take_severed,
};
pub use lockfile::flock_try_exclusive;
pub use map::{FileMap, MapSync, preallocate};
pub use mem::{detected_memory_bound, fadvise_dontneed_all, malloc_trim_now, process_rss_bytes};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use poller_ep::Poller;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use poller_kq::Poller;
pub use pty::open_pty;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
pub use random::fill_random;
pub use signal::{SIGINT, SIGTERM, SIGXFSZ, install_signal_handler};
pub use socket::Socket;
pub use term::{RawMode, terminal_columns};
pub use wait::wait_readable;
pub use waker::Waker;

// ---- Poller ----------------------------------------------------------------

/// A readiness notification for one file descriptor.
///
/// Built by [`Poller::wait`]; the fields are for reading.
///
/// ```
/// fn closing(ev: &kevy_sys::Event) -> bool {
///     ev.hup
/// }
/// # let _ = closing;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Event {
    /// The file descriptor the event fired on.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Waker};
    ///
    /// let (poller, a, b) = (Poller::new()?, Waker::new()?, Waker::new()?);
    /// poller.add(a.read_fd(), Interest::READ)?;
    /// poller.add(b.read_fd(), Interest::READ)?;
    /// b.wake()?;
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, Some(1000))?;
    /// // only the descriptor that became ready is named
    /// assert!(!events.is_empty());
    /// assert!(events.iter().all(|ev| ev.fd == b.read_fd()));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fd: i32,
    /// A `read`/`accept` on `fd` would not block.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Socket};
    ///
    /// let listener = Socket::tcp_listen([127, 0, 0, 1], 0, 16)?;
    /// let poller = Poller::new()?;
    /// poller.add(listener.raw(), Interest::READ)?;
    /// let _client = std::net::TcpStream::connect(("127.0.0.1", listener.local_port()?))?;
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, Some(2000))?;
    /// // a pending connection makes the listener readable: accept won't block
    /// assert!(events.iter().any(|ev| ev.fd == listener.raw() && ev.readable));
    /// let _conn = listener.accept()?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub readable: bool,
    /// A `write` on `fd` would not block.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Socket};
    ///
    /// let listener = Socket::tcp_listen([127, 0, 0, 1], 0, 16)?;
    /// let _client = std::net::TcpStream::connect(("127.0.0.1", listener.local_port()?))?;
    /// let conn = listener.accept()?;
    /// let poller = Poller::new()?;
    /// poller.add(conn.raw(), Interest::WRITE)?;
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, Some(2000))?;
    /// // a fresh connection has an empty send buffer
    /// assert!(events.iter().any(|ev| ev.fd == conn.raw() && ev.writable));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub writable: bool,
    /// Peer hang-up / error — the connection should be closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Socket};
    ///
    /// let listener = Socket::tcp_listen([127, 0, 0, 1], 0, 16)?;
    /// let client = std::net::TcpStream::connect(("127.0.0.1", listener.local_port()?))?;
    /// let conn = listener.accept()?;
    /// let poller = Poller::new()?;
    /// poller.add(conn.raw(), Interest::READ)?;
    /// drop(client); // the peer goes away
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, Some(2000))?;
    /// assert!(events.iter().any(|ev| ev.fd == conn.raw() && ev.hup));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub hup: bool,
}

/// Which readiness a [`Poller`] watches a descriptor for.
///
/// A set of [`Interest::READ`] and [`Interest::WRITE`], combined with `|`.
/// [`Interest::NONE`] keeps the descriptor registered while reporting
/// nothing but hang-ups.
///
/// ```
/// use kevy_sys::Interest;
///
/// let both = Interest::READ | Interest::WRITE;
/// assert!(both.is_readable() && both.is_writable());
/// assert!(!Interest::READ.is_writable());
/// assert!(!Interest::NONE.is_readable());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interest(u8);

impl Interest {
    /// Neither readable nor writable.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Waker};
    ///
    /// let (poller, w) = (Poller::new()?, Waker::new()?);
    /// poller.add(w.read_fd(), Interest::NONE)?; // registered, but muted
    /// w.wake()?;
    /// let mut events = Vec::new();
    /// assert_eq!(poller.wait(&mut events, Some(50))?, 0);
    /// assert_eq!(Interest::NONE, Interest::default());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub const NONE: Self = Self(0);
    /// Readable: a `read`/`accept` would not block.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Waker};
    ///
    /// let (poller, w) = (Poller::new()?, Waker::new()?);
    /// poller.add(w.read_fd(), Interest::READ)?;
    /// w.wake()?;
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, Some(1000))?;
    /// assert!(events.iter().any(|ev| ev.fd == w.read_fd() && ev.readable));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub const READ: Self = Self(1);
    /// Writable: a `write` would not block.
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_sys::{Interest, Poller, Socket};
    ///
    /// let listener = Socket::tcp_listen([127, 0, 0, 1], 0, 16)?;
    /// let _client = std::net::TcpStream::connect(("127.0.0.1", listener.local_port()?))?;
    /// let conn = listener.accept()?;
    /// let poller = Poller::new()?;
    /// // watch for writability only once there is something queued to send
    /// poller.add(conn.raw(), Interest::NONE)?;
    /// poller.modify(conn.raw(), Interest::WRITE)?;
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, Some(2000))?;
    /// assert!(events.iter().any(|ev| ev.fd == conn.raw() && ev.writable));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub const WRITE: Self = Self(2);

    /// Whether this set includes [`Interest::READ`].
    ///
    /// ```
    /// assert!(kevy_sys::Interest::READ.is_readable());
    /// ```
    #[must_use]
    pub const fn is_readable(self) -> bool {
        self.0 & Self::READ.0 != 0
    }

    /// Whether this set includes [`Interest::WRITE`].
    ///
    /// ```
    /// assert!(kevy_sys::Interest::WRITE.is_writable());
    /// ```
    #[must_use]
    pub const fn is_writable(self) -> bool {
        self.0 & Self::WRITE.0 != 0
    }
}

impl core::ops::BitOr for Interest {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for Interest {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// How many raw events to pull from the kernel per `wait` call.
const WAIT_CAPACITY: usize = 1024;

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Event>();
    send_sync::<Interest>();
    send_sync::<Poller>();
    send_sync::<Socket>();
    send_sync::<Waker>();
    send_sync::<FileMap>();
    send_sync::<MapSync>();
    send_sync::<RawMode>();
};

#[cfg(test)]
mod tests;
