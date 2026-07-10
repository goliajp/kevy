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

pub(crate) mod addr;
pub(crate) mod ffi;
mod signal;
mod socket;
mod waker;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod poller_kq;
#[cfg(target_os = "linux")]
mod poller_ep;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use poller_kq::Poller;
#[cfg(target_os = "linux")]
pub use poller_ep::Poller;
pub use signal::{SIGINT, SIGTERM, SIGXFSZ, install_signal_handler};
pub use socket::{Socket, tcp_listen, tcp_listen_reuseport, unix_listen};
pub use waker::{Waker, waker};

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
