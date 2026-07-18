//! Replica-source listener for embed-as-writer.
//!
//! When `Config::embed_writer_listen_addr` is set, every commit on
//! this embed pushes its argv into a shared [`ReplicationSource`]
//! backlog. A background accept loop binds the configured port; each
//! accepted replica gets its own thread that runs the
//! `REPLICATE FROM` handshake and then streams frames from the
//! source until the peer drops.
//!
//! Scope:
//! - **Snapshot ship at handshake only.** A fresh replica (offset 0
//!   against non-empty history) or one whose offset fell past the
//!   backlog gets a full keyspace snapshot at handshake, then live
//!   frames from the snapshot's as-of offset. If a live stream falls
//!   past the backlog mid-stream, the link is closed so the replica
//!   reconnects and takes the snapshot path.
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
use kevy_replicate::wire::{
    SNAPSHOT_CHUNK_MAX, encode_snapshot_begin, encode_snapshot_chunk, encode_snapshot_end,
};

/// Provider that freezes a point-in-time snapshot of the WHOLE embed
/// keyspace: returns `(payload, ack_offset)` where `ack_offset` is
/// the source's `next_offset` at freeze time — live streaming resumes
/// there (kevy-replicate snapshot.md semantics, matching the server
/// primary's pump).
pub(crate) type SnapshotProvider = Arc<dyn Fn() -> (Vec<u8>, u64) + Send + Sync>;
use kevy_replicate::source::{FromOffset, ReplicationSource};
use kevy_resp::Argv;

/// Replication source attached to this embed when it is a scope
/// writer. Pushes from `commit_write` flow into the source;
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
    pub(crate) fn spawn(
        listen_addr: &str,
        backlog_bytes: usize,
        snapshot: SnapshotProvider,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr)?;
        listener.set_nonblocking(true)?;
        let bound_addr = listener.local_addr()?;
        let source = Arc::new(Mutex::new(ReplicationSource::new(backlog_bytes.max(64 * 1024))));
        // Generation of this writer's offset history. The embed
        // writer's backlog is in-memory only — every process boot
        // starts a NEW offset history at 0 — so the generation is
        // minted per-boot as nanos since epoch: unique across
        // restarts without persisting a sidecar the in-memory
        // backlog wouldn't honour anyway. The handshake fence uses
        // it to refuse offset-resume claims from a previous boot's
        // history (offset aliasing).
        let generation = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| (d.as_nanos() as u64).max(1));
        let stop = Arc::new(AtomicBool::new(false));
        let conn_joins = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));

        let source_c = Arc::clone(&source);
        let stop_c = Arc::clone(&stop);
        let conn_joins_c = Arc::clone(&conn_joins);
        let accept_join = thread::Builder::new()
            .name("kevy-embedded-writer-accept".into())
            .spawn(move || {
                run_accept_loop(listener, source_c, generation, stop_c, conn_joins_c, snapshot);
            })
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

    /// Bound listener address (`port = 0` ⇒ OS picks a port; the
    /// caller reads it back via [`crate::Store::writer_addr`]).
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
    generation: u64,
    stop: Arc<AtomicBool>,
    conn_joins: Arc<Mutex<Vec<JoinHandle<()>>>>,
    snapshot: SnapshotProvider,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let source_c = Arc::clone(&source);
                let stop_c = Arc::clone(&stop);
                let snapshot_c = Arc::clone(&snapshot);
                let join = thread::Builder::new()
                    .name("kevy-embedded-writer-conn".into())
                    .spawn(move || run_conn(stream, source_c, generation, stop_c, snapshot_c))
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

fn run_conn(
    mut stream: TcpStream,
    source: Arc<Mutex<ReplicationSource>>,
    generation: u64,
    stop: Arc<AtomicBool>,
    snapshot: SnapshotProvider,
) {
    let Some(req) = handshake_and_prepare(&mut stream) else {
        return;
    };
    let Some(from_offset) =
        ship_snapshot_if_needed(&mut stream, &source, &snapshot, generation, &req)
    else {
        return;
    };
    // Streaming loop. Lock the source briefly each round to clone
    // any pending frame bytes, then release before writing to the
    // socket. Sleep when caught up so we don't busy-spin.
    let mut sent_offset = from_offset;
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
                // No mid-stream snapshot ship; close the link so the
                // replica reconnects — the handshake path ships a
                // snapshot when the requested offset has fallen past
                // the backlog.
                let _ = stream.shutdown(Shutdown::Both);
                break;
            }
        }
    }
}

/// Read the replica's handshake, then flip the socket to blocking:
/// the accept loop's listener is non-blocking and accepted sockets
/// INHERIT that on some platforms (BSD/macOS semantics) — flip to
/// blocking BEFORE any bulk write, or a snapshot ship dies with
/// EWOULDBLOCK the moment the socket buffer fills (measured: EOF at
/// ~319KB, one buffer's worth). Returns the parsed request.
fn handshake_and_prepare(stream: &mut TcpStream) -> Option<kevy_replicate::handshake::HandshakeReq> {
    if stream.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
        return None;
    }
    let req = read_handshake(stream)?;
    if stream.set_nonblocking(false).is_err() {
        return None;
    }
    let _ = stream.set_read_timeout(None);
    Some(req)
}

/// Snapshot path: a fresh replica (offset 0 against a non-empty
/// history), one that fell past the backlog, or one whose claimed
/// generation isn't THIS boot's (offsets alias across boots — the
/// in-memory backlog restarts at 0 every process start) gets the
/// keyspace shipped, then live frames from the snapshot's as-of
/// offset — same fence discipline as the server primary's pump.
/// Returns the live-stream starting offset; `None` = socket error
/// (caller drops the connection).
fn ship_snapshot_if_needed(
    stream: &mut TcpStream,
    source: &Arc<Mutex<ReplicationSource>>,
    snapshot: &SnapshotProvider,
    generation: u64,
    req: &kevy_replicate::handshake::HandshakeReq,
) -> Option<u64> {
    let from_offset = req.from_offset;
    // Generation fence: a resume claim is only honoured within THIS
    // boot's history. `gen 0 + offset 0` is the fresh no-claim form —
    // it falls through to the existing offset rules.
    let gen_mismatch = req.generation != generation
        && !(req.generation == 0 && req.from_offset == 0);
    let needs_snapshot = {
        let g = source.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = g.next_offset();
        gen_mismatch || (from_offset == 0 && next > 0) || g.frames_from(from_offset).is_err()
    };
    if !needs_snapshot {
        if stream.write_all(&encode_ack(generation, from_offset)).is_err() {
            return None;
        }
        return Some(from_offset);
    }
    let (payload, ack_offset) = snapshot();
    if stream.write_all(&encode_ack(generation, ack_offset)).is_err() {
        return None;
    }
    if stream.write_all(&encode_snapshot_begin()).is_err() {
        return None;
    }
    for chunk in payload.chunks(SNAPSHOT_CHUNK_MAX) {
        if stream.write_all(&encode_snapshot_chunk(chunk)).is_err() {
            return None;
        }
    }
    if stream.write_all(&encode_snapshot_end(ack_offset)).is_err() {
        return None;
    }
    Some(ack_offset)
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

/// Read one `REPLICATE FROM <gen> <offset> ID <id>` command off
/// `stream`, return the parsed request. None on any read / parse
/// error — caller drops the connection.
fn read_handshake(stream: &mut TcpStream) -> Option<kevy_replicate::handshake::HandshakeReq> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Ok(Some((argv, _consumed))) = kevy_resp::parse_command(&buf.clone()) {
            return parse_replicate_from(&argv).ok();
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
}

/// Freeze every shard's COW view + the source offset as ONE point in
/// time (all shard locks held across both — `commit_write` pushes
/// into the backlog under a shard lock, so nothing lands between the
/// offset read and the freeze), then serialize outside the locks.
pub(crate) fn freeze_and_serialize(shards: &crate::store::Shards) -> (Vec<u8>, u64) {
    use crate::store::lock_write;
    let guards: Vec<_> = shards.iter().map(|s| lock_write(s)).collect();
    let ack = guards
        .first()
        .and_then(|g| g.writer_source.as_ref())
        .map(|src| {
            src.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .next_offset()
        })
        .unwrap_or(0);
    let views: Vec<kevy_store::SnapshotView> =
        guards.iter().map(|g| g.store.collect_snapshot()).collect();
    drop(guards);

    struct Multi<'v>(&'v [kevy_store::SnapshotView]);
    impl kevy_persist::SnapshotSource for Multi<'_> {
        fn for_each_entry(&self, mut f: impl FnMut(&[u8], &kevy_store::Value, Option<u64>)) {
            for v in self.0 {
                kevy_persist::SnapshotSource::for_each_entry(v, &mut f);
            }
        }
        fn for_each_hash_ttl(&self, mut f: impl FnMut(&[u8], &[u8], u64)) {
            for v in self.0 {
                kevy_persist::SnapshotSource::for_each_hash_ttl(v, &mut f);
            }
        }
    }
    let mut payload = Vec::new();
    // Serialize the whole snapshot into memory first, matching the
    // server pump's posture (streaming straight to the socket is a
    // follow-up on both ends).
    let _ = kevy_persist::write_snapshot_to(&Multi(&views), &mut payload);
    (payload, ack)
}

