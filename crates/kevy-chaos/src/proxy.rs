//! `ChaosProxy` — a pure-std TCP forwarder for injecting network partitions
//! between kevy nodes.
//!
//! Sits between two nodes (`listen A' -> connect A`) so a test can cut,
//! half-open, or delay the link without touching either process:
//!
//! - [`ChaosProxy::cut`] — full bidirectional partition: kills every live
//!   connection and refuses new ones.
//! - [`ChaosProxy::cut_dir`] — **asymmetric** partition: bytes flowing in the
//!   given direction are read and discarded (black-holed) while the opposite
//!   direction keeps flowing. The classic killer for election protocols.
//! - [`ChaosProxy::delay`] — coarse per-chunk latency injection.
//! - [`ChaosProxy::heal`] — clears cut/black-hole modes (not delay).
//!
//! Each proxied connection runs two forwarder threads (one per direction);
//! the accept loop polls a nonblocking listener against a shutdown flag so
//! `Drop` can join everything cleanly.

// Teardown. `join` returns whatever the thread panicked with, and the
// thread is already being abandoned; `shutdown` on a socket the peer
// has closed reports what already happened. Neither has a caller left
// to tell, and stopping halfway through a teardown leaves more behind
// than finishing it blind.
#![expect(clippy::let_underscore_must_use, reason = "teardown has nobody left to report to")]

use std::io::Write as _;
use std::io::{self, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Full bidirectional cut: existing connections killed, new ones refused.
const MODE_CUT: u8 = 0b001;
/// Black-hole bytes flowing client -> upstream.
const MODE_BLACKHOLE_UP: u8 = 0b010;
/// Black-hole bytes flowing upstream -> client.
const MODE_BLACKHOLE_DOWN: u8 = 0b100;

/// Poll interval for the nonblocking accept loop.
const ACCEPT_POLL: Duration = Duration::from_millis(2);

/// Direction of a proxied byte stream, for [`ChaosProxy::cut_dir`].
///
/// ```
/// # use std::io::{Read, Write};
/// # let upstream = std::net::TcpListener::bind("127.0.0.1:0")?;
/// # let upstream_addr = upstream.local_addr()?;
/// # let (seen_tx, seen) = std::sync::mpsc::channel::<Vec<u8>>();
/// # std::thread::spawn(move || {
/// #     for mut s in upstream.incoming().flatten() {
/// #         let seen_tx = seen_tx.clone();
/// #         std::thread::spawn(move || {
/// #             let mut b = [0u8; 64];
/// #             while let Ok(n @ 1..) = s.read(&mut b) {
/// #                 let _ = seen_tx.send(b[..n].to_vec());
/// #                 if s.write_all(&b[..n]).is_err() { break; }
/// #             }
/// #         });
/// #     }
/// # });
/// // `upstream_addr` is an echo server; `seen` receives every chunk it reads
/// use kevy_chaos::{ChaosProxy, Direction};
/// use std::time::Duration;
///
/// let proxy = ChaosProxy::spawn("127.0.0.1:0", upstream_addr)?;
/// let mut c = std::net::TcpStream::connect(proxy.listen_addr())?;
/// c.set_read_timeout(Some(Duration::from_millis(200)))?;
/// let mut buf = [0u8; 4];
///
/// // either direction can be black-holed on its own, and healed again
/// for dir in [Direction::ToUpstream, Direction::ToDownstream] {
///     proxy.cut_dir(dir);
///     c.write_all(b"ping")?;
///     assert!(c.read(&mut buf).is_err(), "no echo while {dir:?} is cut");
///     proxy.heal();
/// #   while seen.try_recv().is_ok() {}
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Bytes from the downstream client toward the upstream server.
    ///
    /// ```
    /// # use std::io::{Read, Write};
    /// # let upstream = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// # let upstream_addr = upstream.local_addr()?;
    /// # let (seen_tx, seen) = std::sync::mpsc::channel::<Vec<u8>>();
    /// # std::thread::spawn(move || {
    /// #     for mut s in upstream.incoming().flatten() {
    /// #         let seen_tx = seen_tx.clone();
    /// #         std::thread::spawn(move || {
    /// #             let mut b = [0u8; 64];
    /// #             while let Ok(n @ 1..) = s.read(&mut b) {
    /// #                 let _ = seen_tx.send(b[..n].to_vec());
    /// #                 if s.write_all(&b[..n]).is_err() { break; }
    /// #             }
    /// #         });
    /// #     }
    /// # });
    /// // `upstream_addr` is an echo server; `seen` receives every chunk it reads
    /// use kevy_chaos::{ChaosProxy, Direction};
    /// use std::time::Duration;
    ///
    /// let proxy = ChaosProxy::spawn("127.0.0.1:0", upstream_addr)?;
    /// let mut c = std::net::TcpStream::connect(proxy.listen_addr())?;
    /// c.set_read_timeout(Some(Duration::from_millis(200)))?;
    /// let mut buf = [0u8; 4];
    ///
    /// proxy.cut_dir(Direction::ToUpstream);
    /// c.write_all(b"ping")?;
    /// // the write succeeded, but the server never sees it
    /// assert!(seen.recv_timeout(Duration::from_millis(200)).is_err());
    /// assert!(c.read(&mut buf).is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ToUpstream,
    /// Bytes from the upstream server toward the downstream client.
    ///
    /// ```
    /// # use std::io::{Read, Write};
    /// # let upstream = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// # let upstream_addr = upstream.local_addr()?;
    /// # let (seen_tx, seen) = std::sync::mpsc::channel::<Vec<u8>>();
    /// # std::thread::spawn(move || {
    /// #     for mut s in upstream.incoming().flatten() {
    /// #         let seen_tx = seen_tx.clone();
    /// #         std::thread::spawn(move || {
    /// #             let mut b = [0u8; 64];
    /// #             while let Ok(n @ 1..) = s.read(&mut b) {
    /// #                 let _ = seen_tx.send(b[..n].to_vec());
    /// #                 if s.write_all(&b[..n]).is_err() { break; }
    /// #             }
    /// #         });
    /// #     }
    /// # });
    /// // `upstream_addr` is an echo server; `seen` receives every chunk it reads
    /// use kevy_chaos::{ChaosProxy, Direction};
    /// use std::time::Duration;
    ///
    /// let proxy = ChaosProxy::spawn("127.0.0.1:0", upstream_addr)?;
    /// let mut c = std::net::TcpStream::connect(proxy.listen_addr())?;
    /// c.set_read_timeout(Some(Duration::from_millis(200)))?;
    /// let mut buf = [0u8; 4];
    ///
    /// proxy.cut_dir(Direction::ToDownstream);
    /// c.write_all(b"ping")?;
    /// // the server gets the request, but its reply is dropped
    /// assert_eq!(seen.recv_timeout(Duration::from_secs(5))?, b"ping");
    /// assert!(c.read(&mut buf).is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ToDownstream,
}

impl Direction {
    fn blackhole_bit(self) -> u8 {
        match self {
            Direction::ToUpstream => MODE_BLACKHOLE_UP,
            Direction::ToDownstream => MODE_BLACKHOLE_DOWN,
        }
    }
}

/// Control-plane state shared with the accept loop and forwarder threads.
struct Shared {
    mode: AtomicU8,
    delay_ms: AtomicU64,
    shutdown: AtomicBool,
    /// Registry of live proxied sockets (both sides of every connection),
    /// so `cut()` / `Drop` can unblock forwarders parked in `read()`.
    conns: Mutex<Vec<TcpStream>>,
}

impl Shared {
    fn kill_connections(&self) {
        let mut conns = self
            .conns
            .lock()
            .expect("the lock is only poisoned by a panic that already failed the process");
        for stream in conns.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Pure-std TCP chaos proxy. See the module docs for the injection model.
///
/// Dropping the proxy shuts down the listener, kills every proxied
/// connection, and joins all threads.
///
/// ```
/// # use std::io::{Read, Write};
/// # let upstream = std::net::TcpListener::bind("127.0.0.1:0")?;
/// # let upstream_addr = upstream.local_addr()?;
/// # let (seen_tx, seen) = std::sync::mpsc::channel::<Vec<u8>>();
/// # std::thread::spawn(move || {
/// #     for mut s in upstream.incoming().flatten() {
/// #         let seen_tx = seen_tx.clone();
/// #         std::thread::spawn(move || {
/// #             let mut b = [0u8; 64];
/// #             while let Ok(n @ 1..) = s.read(&mut b) {
/// #                 let _ = seen_tx.send(b[..n].to_vec());
/// #                 if s.write_all(&b[..n]).is_err() { break; }
/// #             }
/// #         });
/// #     }
/// # });
/// // `upstream_addr` is an echo server; `seen` receives every chunk it reads
/// use kevy_chaos::{ChaosProxy, Direction};
/// use std::time::Duration;
///
/// let proxy = ChaosProxy::spawn("127.0.0.1:0", upstream_addr)?;
/// let mut c = std::net::TcpStream::connect(proxy.listen_addr())?;
/// c.set_read_timeout(Some(Duration::from_millis(200)))?;
/// let mut buf = [0u8; 4];
///
/// c.write_all(b"ping")?;
/// c.read_exact(&mut buf)?;
/// assert_eq!(&buf, b"ping");
///
/// // a full cut kills the live connection
/// proxy.cut();
/// assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
///
/// // after heal, new connections go through again, here with added latency
/// proxy.heal();
/// proxy.delay(Duration::from_millis(5));
/// let mut c = std::net::TcpStream::connect(proxy.listen_addr())?;
/// c.set_read_timeout(Some(Duration::from_secs(5)))?;
/// c.write_all(b"pong")?;
/// c.read_exact(&mut buf)?;
/// assert_eq!(&buf, b"pong");
/// # drop(seen);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct ChaosProxy {
    shared: Arc<Shared>,
    accept_thread: Option<JoinHandle<()>>,
    listen_addr: SocketAddr,
}

impl ChaosProxy {
    /// Bind `listen_addr` and forward every inbound connection to
    /// `upstream_addr`. Pass port 0 to let the OS pick; the resolved address
    /// is available via [`ChaosProxy::listen_addr`].
    pub fn spawn(
        listen_addr: impl ToSocketAddrs,
        upstream_addr: impl ToSocketAddrs,
    ) -> io::Result<ChaosProxy> {
        let listener = TcpListener::bind(listen_addr)?;
        listener.set_nonblocking(true)?;
        let listen_addr = listener.local_addr()?;
        let upstream = upstream_addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "upstream_addr resolved to no address")
        })?;
        let shared = Arc::new(Shared {
            mode: AtomicU8::new(0),
            delay_ms: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            conns: Mutex::new(Vec::new()),
        });
        let shared2 = Arc::clone(&shared);
        let accept_thread = thread::spawn(move || accept_loop(&listener, upstream, &shared2));
        Ok(ChaosProxy { shared, accept_thread: Some(accept_thread), listen_addr })
    }

    /// The address the proxy is listening on (useful with port 0).
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Full bidirectional partition: kill every live proxied connection and
    /// refuse new ones (accepted then immediately dropped) until [`heal`].
    ///
    /// [`heal`]: ChaosProxy::heal
    pub fn cut(&self) {
        self.shared.mode.fetch_or(MODE_CUT, Ordering::Relaxed);
        self.shared.kill_connections();
    }

    /// Clear all cut / black-hole modes. Live connections that survived a
    /// directional cut resume forwarding; `delay` is left untouched.
    pub fn heal(&self) {
        self.shared.mode.store(0, Ordering::Relaxed);
    }

    /// Asymmetric partition: bytes flowing in `dir` are read and discarded
    /// (black-holed) while the opposite direction keeps flowing. Connections
    /// stay open — the sender sees successful writes that never arrive.
    pub fn cut_dir(&self, dir: Direction) {
        self.shared.mode.fetch_or(dir.blackhole_bit(), Ordering::Relaxed);
    }

    /// Sleep this long before forwarding each chunk, in both directions.
    /// Millisecond granularity; `Duration::ZERO` disables.
    pub fn delay(&self, delay: Duration) {
        self.shared.delay_ms.store(delay.as_millis() as u64, Ordering::Relaxed);
    }
}

impl Drop for ChaosProxy {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.kill_connections();
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, upstream: SocketAddr, shared: &Arc<Shared>) {
    let mut forwarders: Vec<JoinHandle<()>> = Vec::new();
    while !shared.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((client, _peer)) => {
                // On BSD/macOS the accepted socket inherits the listener's
                // O_NONBLOCK; forwarders need blocking reads.
                if client.set_nonblocking(false).is_err() {
                    continue;
                }
                forwarders.retain(|handle| !handle.is_finished());
                if shared.mode.load(Ordering::Relaxed) & MODE_CUT != 0 {
                    drop(client); // refuse: peer sees EOF/reset on first read
                    continue;
                }
                match TcpStream::connect(upstream) {
                    Ok(up) => spawn_forwarders(client, up, shared, &mut forwarders),
                    Err(_) => drop(client),
                }
            }
            // WouldBlock (nonblocking listener idle) or transient error.
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
    // Streams were shut down by Drop's kill_connections; forwarders exit fast.
    for handle in forwarders {
        let _ = handle.join();
    }
}

fn spawn_forwarders(
    client: TcpStream,
    upstream: TcpStream,
    shared: &Arc<Shared>,
    forwarders: &mut Vec<JoinHandle<()>>,
) {
    let clones =
        (client.try_clone(), upstream.try_clone(), client.try_clone(), upstream.try_clone());
    let (Ok(c_wr), Ok(u_wr), Ok(c_reg), Ok(u_reg)) = clones else {
        return; // clone failed: both originals drop => connection refused
    };
    {
        let mut conns = shared
            .conns
            .lock()
            .expect("the lock is only poisoned by a panic that already failed the process");
        conns.push(c_reg);
        conns.push(u_reg);
    }
    // Close the register-vs-cut race: if cut() drained the registry between
    // our accept-time check and the push above, kill what we just added.
    if shared.mode.load(Ordering::Relaxed) & MODE_CUT != 0 {
        shared.kill_connections();
    }
    let shared_up = Arc::clone(shared);
    forwarders
        .push(thread::spawn(move || forward(client, u_wr, Direction::ToUpstream, &shared_up)));
    let shared_down = Arc::clone(shared);
    forwarders.push(thread::spawn(move || {
        forward(upstream, c_wr, Direction::ToDownstream, &shared_down)
    }));
}

/// One direction of one proxied connection. Checks the control plane before
/// forwarding each chunk; exits on EOF, error, shutdown, or full cut.
fn forward(mut from: TcpStream, mut to: TcpStream, dir: Direction, shared: &Shared) {
    let mut buf = [0u8; 8192];
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mode = shared.mode.load(Ordering::Relaxed);
        if mode & MODE_CUT != 0 {
            break;
        }
        let delay_ms = shared.delay_ms.load(Ordering::Relaxed);
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
        if mode & dir.blackhole_bit() != 0 {
            continue; // black hole: bytes read and discarded
        }
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    // Propagate the half-close so the peer's read side sees EOF.
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
