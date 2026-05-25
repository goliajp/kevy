//! kevy-net — a single-threaded, event-driven reactor.
//!
//! Built on the [kevy-sys] readiness poller (kqueue/epoll). Connections are
//! non-blocking; the loop multiplexes thousands of them on one thread. The
//! application plugs in via the byte-level [`Service`] trait and never sees
//! readiness, file descriptors, or the poller — so a future io_uring
//! (completion-model) engine could drive the same `Service` unchanged. This is
//! the orthodox foundation that thread-per-core sharding replicates per core.
//!
//! Part of the [kevy] key–value server.
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-sys]: https://crates.io/crates/kevy-sys
//!
//! # Example
//!
//! A [`Service`] turns inbound bytes into reply bytes; the reactor owns the
//! sockets and the loop. Here is an echo service:
//!
//! ```
//! use kevy_net::Service;
//!
//! struct Echo;
//! impl Service for Echo {
//!     fn on_data(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) -> bool {
//!         output.append(input); // move all bytes to the reply; keep the conn open
//!         true
//!     }
//! }
//!
//! let mut svc = Echo;
//! let mut input = b"ping".to_vec();
//! let mut output = Vec::new();
//! assert!(svc.on_data(&mut input, &mut output));
//! assert_eq!(output, b"ping");
//! assert!(input.is_empty());
//! ```
//!
//! To serve it, wrap a `kevy_sys` listener:
//! `Reactor::new(listener)?.run(&mut svc, &stop)`.
#![forbid(unsafe_code)]

use kevy_sys::{Event, Poller, Socket};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Application logic, driven by the reactor one connection at a time.
pub trait Service {
    /// Process every complete request buffered in `input`, appending replies to
    /// `output`. Consume parsed bytes from `input`; leave any partial frame.
    /// Return `false` to close the connection once `output` has been flushed.
    fn on_data(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) -> bool;
}

/// Per-connection state owned by the reactor.
struct Conn {
    sock: Socket,
    input: Vec<u8>,
    output: Vec<u8>,
    /// How much of `output` has already been written.
    write_pos: usize,
    /// Whether we currently ask the poller for write-readiness.
    want_write: bool,
    /// Close as soon as `output` is fully flushed.
    close_after_flush: bool,
}

impl Conn {
    fn has_pending_output(&self) -> bool {
        self.write_pos < self.output.len()
    }
}

/// The reactor: one listener, a poller, and a table of live connections.
pub struct Reactor {
    poller: Poller,
    listener: Socket,
    conns: HashMap<i32, Conn>,
    events: Vec<Event>,
    read_buf: Vec<u8>,
}

/// How long `wait` blocks before re-checking the stop flag when idle.
const IDLE_TICK_MS: i32 = 100;

impl Reactor {
    /// Wrap a (blocking) listener: switch it to non-blocking and register it.
    pub fn new(listener: Socket) -> io::Result<Self> {
        listener.set_nonblocking()?;
        let poller = Poller::new()?;
        poller.add(listener.raw(), true, false)?;
        Ok(Reactor {
            poller,
            listener,
            conns: HashMap::new(),
            events: Vec::with_capacity(1024),
            read_buf: vec![0u8; 64 * 1024],
        })
    }

    /// Run the loop until `stop` is set. Returns on a fatal poller error.
    pub fn run<S: Service>(&mut self, service: &mut S, stop: &AtomicBool) -> io::Result<()> {
        while !stop.load(Ordering::Relaxed) {
            self.poller.wait(&mut self.events, Some(IDLE_TICK_MS))?;
            // Move the events out so we can take `&mut self` while iterating.
            let events = std::mem::take(&mut self.events);
            for ev in &events {
                if ev.fd == self.listener.raw() {
                    self.accept_ready()?;
                } else {
                    self.conn_ready(*ev, service)?;
                }
            }
            self.events = events; // reuse the allocation next iteration
        }
        Ok(())
    }

    /// Drain the listener's backlog, registering each new connection.
    fn accept_ready(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok(sock) => {
                    sock.set_nonblocking()?;
                    let _ = sock.set_nodelay(); // best-effort latency tweak
                    let fd = sock.raw();
                    self.poller.add(fd, true, false)?;
                    self.conns.insert(
                        fd,
                        Conn {
                            sock,
                            input: Vec::new(),
                            output: Vec::new(),
                            write_pos: 0,
                            want_write: false,
                            close_after_flush: false,
                        },
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // transient; retry on the next readiness tick
            }
        }
        Ok(())
    }

    /// Service one connection's readiness event.
    fn conn_ready<S: Service>(&mut self, ev: Event, service: &mut S) -> io::Result<()> {
        let close;
        let want_write;
        {
            let Some(conn) = self.conns.get_mut(&ev.fd) else {
                return Ok(());
            };
            let mut should_close = ev.hup;

            if ev.readable && !should_close {
                loop {
                    match conn.sock.read(&mut self.read_buf) {
                        Ok(0) => {
                            should_close = true;
                            break;
                        }
                        Ok(n) => conn.input.extend_from_slice(&self.read_buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            should_close = true;
                            break;
                        }
                    }
                }
                if !conn.input.is_empty() && !service.on_data(&mut conn.input, &mut conn.output) {
                    conn.close_after_flush = true;
                }
            }

            flush(conn);

            if conn.close_after_flush && !conn.has_pending_output() {
                should_close = true;
            }
            close = should_close;
            want_write = conn.has_pending_output();
        }

        if close {
            self.poller.delete(ev.fd)?;
            self.conns.remove(&ev.fd); // Socket's Drop closes the fd
            return Ok(());
        }

        // Register/clear write-interest only when it actually changes.
        if let Some(conn) = self.conns.get_mut(&ev.fd)
            && want_write != conn.want_write
        {
            conn.want_write = want_write;
            self.poller.modify(ev.fd, true, want_write)?;
        }
        Ok(())
    }
}

/// Write as much of `output` as the socket will currently take.
fn flush(conn: &mut Conn) {
    while conn.write_pos < conn.output.len() {
        match conn.sock.write(&conn.output[conn.write_pos..]) {
            Ok(0) => break,
            Ok(n) => conn.write_pos += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                conn.close_after_flush = true;
                break;
            }
        }
    }
    if conn.write_pos == conn.output.len() {
        conn.output.clear();
        conn.write_pos = 0;
    }
}
