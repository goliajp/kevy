//! Replica-source listener for embed-as-writer (Phase 3 / v1.21).
//!
//! When `Config::embed_writer_listen_addr` is set, every commit on
//! this embed pushes its argv into a shared [`ReplicationSource`]
//! backlog. A background accept loop binds the configured port; each
//! accepted replica gets its own thread that runs the
//! `REPLICATE FROM` handshake and then streams frames from the
//! source until the peer drops.
//!
//! v1.21 MVP scope:
//! - **No snapshot ship from embed yet.** A replica reconnecting at
//!   an offset older than the backlog's oldest gets `TooOld` →
//!   connection closed; the embed's own snapshot serializer landing
//!   here is a follow-up. For now operators size the backlog large
//!   enough to cover plausible reconnect windows (or accept a
//!   re-handshake from offset 0 by the consumer).
//! - **One-shard model.** Embed writes to a single source; the
//!   per-shard split that the server uses is intentionally not
//!   replicated here — an embed-as-writer for a scope is a single-
//!   process logical unit.
//! - **Blocking-only I/O.** No io_uring, no epoll: one OS thread
//!   per accepted replica. Designed for the "scope writer's
//!   replication is a control-plane event, not a hot-path" posture.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kevy_replicate::handshake::{encode_ack, parse_replicate_from};
use kevy_replicate::source::{FromOffset, ReplicationSource};
use kevy_resp::Argv;

/// Replication source attached to this embed when it is a Phase 3
/// scope writer. Pushes from `commit_write` flow into the source;
/// accepted replicas stream out of it.
pub(crate) struct ReplicaSource {
    source: Arc<Mutex<ReplicationSource>>,
    stop: Arc<AtomicBool>,
    /// Local bound address (used in `shutdown` to wake the accept
    /// thread by connecting to itself).
    bound_addr: std::net::SocketAddr,
    accept_join: Mutex<Option<JoinHandle<()>>>,
    conn_joins: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ReplicaSource {
    /// Bind the listener + spawn the accept loop. Returns immediately
    /// — replicas haven't connected yet. The caller stores the
    /// `ReplicaSource` in `DropGuard` so the threads are joined on
    /// last-clone drop.
    pub(crate) fn spawn(listen_addr: &str, backlog_bytes: usize) -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr)?;
        listener.set_nonblocking(true)?;
        let bound_addr = listener.local_addr()?;
        let source = Arc::new(Mutex::new(ReplicationSource::new(backlog_bytes.max(64 * 1024))));
        let stop = Arc::new(AtomicBool::new(false));
        let conn_joins = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));

        let source_c = Arc::clone(&source);
        let stop_c = Arc::clone(&stop);
        let conn_joins_c = Arc::clone(&conn_joins);
        let accept_join = thread::Builder::new()
            .name("kevy-embedded-writer-accept".into())
            .spawn(move || run_accept_loop(listener, source_c, stop_c, conn_joins_c))
            .expect("spawn writer-accept thread");

        Ok(Self {
            source,
            stop,
            bound_addr,
            accept_join: Mutex::new(Some(accept_join)),
            conn_joins,
        })
    }

    /// Append one mutation argv to the backlog. Called from
    /// `commit_write` whenever the local store applies a mutation.
    /// Cheap (one mutex lock + small Vec push); cement layer is
    /// expected to hold this lock briefly.
    ///
    /// Today `commit_write` pushes via the shared
    /// `Arc<Mutex<ReplicationSource>>` cloned into every
    /// shard's `Inner`, so this method is unused on the hot path —
    /// keep it as the documented public surface and a fallback for
    /// callers that don't have `Inner` access (the upcoming
    /// MOVE-SCOPE quiesce path will use it).
    #[allow(dead_code)]
    pub(crate) fn push_argv(&self, parts: &[&[u8]]) {
        push_into(&self.source, parts);
    }

    /// Clone the `Arc<Mutex<ReplicationSource>>` so the shard
    /// `Inner` can push directly without going through the
    /// `ReplicaSource` handle (which is owned by `DropGuard`, behind
    /// an `Arc<...>` itself). Lets `commit_write` push under the
    /// shard lock instead of reaching back up through the store
    /// guard.
    pub(crate) fn shared_source(&self) -> Arc<Mutex<ReplicationSource>> {
        Arc::clone(&self.source)
    }

    /// Bound listener address; mostly useful for tests
    /// (`port = 0` ⇒ OS picks a port; the test reads it back).
    #[allow(dead_code)]
    pub(crate) fn local_addr(&self) -> std::net::SocketAddr {
        self.bound_addr
    }

    /// Stop the accept loop + every connection thread + join. Called
    /// from `DropGuard::drop`.
    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop's non-blocking-poll sleep by
        // connecting to ourselves once. Errors swallowed — even a
        // refused connect bumps the listener's accept queue enough
        // for the next poll cycle to see `stop`.
        let _ = TcpStream::connect_timeout(&self.bound_addr, Duration::from_millis(200));
        if let Some(j) = self
            .accept_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = j.join();
        }
        let mut joins = self
            .conn_joins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for j in joins.drain(..) {
            let _ = j.join();
        }
    }
}

/// Push one argv into the shared source, encoding via `Argv::push`.
/// Lives here so `commit_write` in `store.rs` can call into a
/// helper instead of inlining the lock + argv build.
pub(crate) fn push_into(source: &Arc<Mutex<ReplicationSource>>, parts: &[&[u8]]) {
    let mut g = source
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut argv = Argv::default();
    for p in parts {
        argv.push(p);
    }
    let _offset = g.push_mutation(&argv);
}

fn run_accept_loop(
    listener: TcpListener,
    source: Arc<Mutex<ReplicationSource>>,
    stop: Arc<AtomicBool>,
    conn_joins: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let source_c = Arc::clone(&source);
                let stop_c = Arc::clone(&stop);
                let join = thread::Builder::new()
                    .name("kevy-embedded-writer-conn".into())
                    .spawn(move || run_conn(stream, source_c, stop_c))
                    .expect("spawn writer-conn thread");
                conn_joins
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(join);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Non-blocking accept — pause briefly so `stop` is
                // acted on within `slice`.
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                // Listener went bad (rare); give up the loop so
                // shutdown's join doesn't hang.
                break;
            }
        }
    }
}

fn run_conn(mut stream: TcpStream, source: Arc<Mutex<ReplicationSource>>, stop: Arc<AtomicBool>) {
    if stream.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
        return;
    }
    let from_offset = match read_handshake(&mut stream) {
        Some(off) => off,
        None => return,
    };
    if stream.write_all(&encode_ack(from_offset)).is_err() {
        return;
    }
    // Streaming loop. Lock the source briefly each round to clone
    // any pending frame bytes, then release before writing to the
    // socket. Sleep when caught up so we don't busy-spin.
    let mut sent_offset = from_offset;
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(None);
    while !stop.load(Ordering::Relaxed) {
        let next = next_frame_bytes(&source, sent_offset);
        match next {
            FrameStep::Send(bytes, new_off) => {
                if stream.write_all(&bytes).is_err() {
                    break;
                }
                sent_offset = new_off;
            }
            FrameStep::CaughtUp => {
                thread::sleep(Duration::from_millis(20));
            }
            FrameStep::TooOld | FrameStep::PeerAhead => {
                // No snapshot ship from embed yet (v1.21 anti-scope);
                // close the link so the replica reconnects from 0
                // and rebuilds from the backlog (or the operator
                // sized it bigger).
                let _ = stream.shutdown(Shutdown::Both);
                break;
            }
        }
    }
}

enum FrameStep {
    Send(Vec<u8>, u64),
    CaughtUp,
    TooOld,
    PeerAhead,
}

fn next_frame_bytes(source: &Arc<Mutex<ReplicationSource>>, sent_offset: u64) -> FrameStep {
    let s = source
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sent_offset >= s.next_offset() {
        return FrameStep::CaughtUp;
    }
    match s.frames_from(sent_offset) {
        Ok(mut it) => match it.next() {
            Some(frame) => FrameStep::Send(frame.bytes.clone(), frame.offset.saturating_add(1)),
            None => FrameStep::CaughtUp,
        },
        Err(FromOffset::TooOld) => FrameStep::TooOld,
        Err(FromOffset::Future) => FrameStep::PeerAhead,
    }
}

/// Read one `REPLICATE FROM <offset> ID <id>` command off `stream`,
/// return the from_offset. None on any read / parse error — caller
/// drops the connection.
fn read_handshake(stream: &mut TcpStream) -> Option<u64> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Ok(Some((argv, _consumed))) = kevy_resp::parse_command(&buf.clone()) {
            return parse_replicate_from(&argv).ok().map(|req| req.from_offset);
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
}
