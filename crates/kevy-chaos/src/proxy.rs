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

use std::io::{self, Read};
use std::io::Write as _;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Bytes from the downstream client toward the upstream server.
    ToUpstream,
    /// Bytes from the upstream server toward the downstream client.
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
        let mut conns = self.conns.lock().unwrap();
        for stream in conns.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Pure-std TCP chaos proxy. See the module docs for the injection model.
///
/// Dropping the proxy shuts down the listener, kills every proxied
/// connection, and joins all threads.
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
    let clones = (client.try_clone(), upstream.try_clone(), client.try_clone(), upstream.try_clone());
    let (Ok(c_wr), Ok(u_wr), Ok(c_reg), Ok(u_reg)) = clones else {
        return; // clone failed: both originals drop => connection refused
    };
    {
        let mut conns = shared.conns.lock().unwrap();
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
    forwarders
        .push(thread::spawn(move || forward(upstream, c_wr, Direction::ToDownstream, &shared_down)));
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
mod tests {
    use super::*;
    use std::time::Instant;

    /// Echo server: writes back whatever it reads, one thread per connection.
    fn spawn_echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// Echo server that additionally pushes a `+` heartbeat every 20 ms,
    /// independent of inbound traffic — server->client bytes keep flowing
    /// even when client->server is black-holed.
    fn spawn_heartbeat_echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut hb = stream.try_clone().unwrap();
                thread::spawn(move || {
                    while hb.write_all(b"+").is_ok() {
                        thread::sleep(Duration::from_millis(20));
                    }
                });
                let mut echo_wr = stream.try_clone().unwrap();
                thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if echo_wr.write_all(&buf[..n]).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// Collect whatever arrives on `stream` until `dur` elapses.
    /// Requires a short read timeout already set on the stream.
    fn read_for(stream: &mut TcpStream, dur: Duration) -> Vec<u8> {
        let deadline = Instant::now() + dur;
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
        out
    }

    /// Assert a read result means "connection dead" (EOF or hard error),
    /// never a timeout (which would also match a merely-silent link).
    fn assert_conn_dead(result: io::Result<usize>) {
        match result {
            Ok(0) => {}
            Err(e)
                if e.kind() != io::ErrorKind::WouldBlock
                    && e.kind() != io::ErrorKind::TimedOut => {}
            other => panic!("expected dead connection, got {other:?}"),
        }
    }

    #[test]
    fn passthrough_bytes() {
        let server = spawn_echo_server();
        let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
        let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
        client.write_all(b"hello kevy").unwrap();
        let mut buf = [0u8; 10];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello kevy");
    }

    #[test]
    fn cut_kills_existing_and_refuses_new() {
        let server = spawn_echo_server();
        let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
        let mut old = TcpStream::connect(proxy.listen_addr()).unwrap();
        old.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        old.read_exact(&mut buf).unwrap();

        proxy.cut();

        // Existing connection: killed (EOF or reset), not merely silent.
        old.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut byte = [0u8; 1];
        assert_conn_dead(old.read(&mut byte));

        // New connection: connect may succeed (accept-then-drop) but the
        // socket is dead — first read sees EOF/reset.
        if let Ok(mut fresh) = TcpStream::connect(proxy.listen_addr()) {
            fresh.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            assert_conn_dead(fresh.read(&mut byte));
        }
    }

    #[test]
    fn heal_restores_service() {
        let server = spawn_echo_server();
        let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
        proxy.cut();
        proxy.heal();
        let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
        client.write_all(b"back").unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"back");
    }

    #[test]
    fn cut_dir_to_upstream_black_holes_one_way() {
        let server = spawn_heartbeat_echo_server();
        let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
        let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
        client.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

        // Sanity pre-cut: echo and heartbeats both flow.
        client.write_all(b"AB").unwrap();
        let got = read_for(&mut client, Duration::from_millis(300));
        assert!(got.contains(&b'A') && got.contains(&b'B'), "echo before cut: {got:?}");
        assert!(got.contains(&b'+'), "heartbeat before cut: {got:?}");

        proxy.cut_dir(Direction::ToUpstream);
        // Drain anything echoed before the cut took effect.
        let _ = read_for(&mut client, Duration::from_millis(100));

        // client->server black-holed: write succeeds, echo never comes back;
        // server->client stays open: heartbeats keep arriving. Asymmetry.
        client.write_all(b"XY").unwrap();
        let got = read_for(&mut client, Duration::from_millis(400));
        assert!(got.contains(&b'+'), "server->client must stay open, got {got:?}");
        assert!(
            !got.contains(&b'X') && !got.contains(&b'Y'),
            "client->server bytes must be black-holed, got {got:?}"
        );

        // heal(): the SAME connection resumes (black hole discards, not closes).
        proxy.heal();
        client.write_all(b"Z").unwrap();
        let got = read_for(&mut client, Duration::from_millis(500));
        assert!(got.contains(&b'Z'), "echo after heal: {got:?}");
    }

    #[test]
    fn delay_slows_round_trip() {
        let server = spawn_echo_server();
        let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
        let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
        let mut buf = [0u8; 1];

        // Baseline sanity without delay.
        client.write_all(b"a").unwrap();
        client.read_exact(&mut buf).unwrap();

        proxy.delay(Duration::from_millis(150));
        let start = Instant::now();
        client.write_all(b"b").unwrap();
        client.read_exact(&mut buf).unwrap();
        let elapsed = start.elapsed();
        // 150 ms injected in each direction => >= ~300 ms round trip.
        assert!(elapsed >= Duration::from_millis(280), "round trip too fast: {elapsed:?}");
    }
}
