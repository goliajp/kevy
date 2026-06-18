//! TCP control-plane transport for [`crate::Elector`] — the network
//! half of T1.5.6. Drives the elector by reading inbound frames off
//! one accept-side listener + writing outbound frames over one
//! persistent connection per peer.
//!
//! Architecture: **one thread for the listener** + **one thread per
//! outbound peer** + **one orchestrator thread** that owns the
//! `Elector` and drives `tick` / `on_message` against it. Inbound
//! frames + outbound dispatch + tick fire all flow through MPSC
//! channels into the orchestrator (single-threaded against the
//! elector — no Mutex on the hot path).
//!
//! Sockets are blocking TCP — kevy-elect's traffic is rare
//! (heartbeats at 5 Hz default) so the busy-wait / async machinery
//! that the keyspace plane needs is overkill here. The orchestrator
//! checks the inbound channel with `recv_timeout(hb_interval)` so
//! ticks fire at the configured cadence without burning a core.
//!
//! Out of scope (Phase 1.5): TLS / auth / connection pooling.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, Sender, channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::elector::{Elector, Outbound};
use crate::message::Message;
use crate::wire::{DecodeError, decode, encode};

/// Maximum buffer the per-connection reader holds before declaring
/// the framing busted. Election frames are ≤ 256 B; 16 KiB is
/// generous for misaligned partial reads.
const READ_BUF_CAP: usize = 16 * 1024;

/// Read-loop sleep on transient EAGAIN-equivalents (peer closed,
/// I/O error during decode). Keeps the worker from a tight retry
/// loop while still recovering on reconnect.
const READ_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// One inbound event the orchestrator processes. Either a decoded
/// election message from a peer, or a "the connection from $peer
/// went down" notification (so the orchestrator can clear any
/// state that assumed the link was up).
pub enum InboundEvent {
    /// `(from_node_id, msg)`.
    Message(String, Message),
    /// The accept thread saw a new inbound connection but the
    /// handshake / first-frame read failed. `String` is the peer
    /// addr for diagnostics.
    InboundConnFailed(String),
}

/// Shared state between the orchestrator + worker threads. Wraps
/// the elector in a Mutex so the per-peer outbound threads can read
/// the latest `epoch` / `repl_offset` for the next heartbeat
/// without round-tripping through the orchestrator — but **only the
/// orchestrator mutates** via `tick` / `on_message`.
struct Shared {
    elector: Mutex<Elector>,
    /// Per-peer outbound queue. Indexed by `node_id`. Each worker
    /// drains its own queue + writes onto the persistent TCP
    /// stream; on stream death the queue is held until the worker
    /// reconnects. Bounded by `MAX_PENDING_PER_PEER` to prevent a
    /// dead peer from leaking memory.
    out_queues: Mutex<std::collections::HashMap<String, std::collections::VecDeque<Message>>>,
}

const MAX_PENDING_PER_PEER: usize = 256;

/// Per-peer addressing. Maps `node_id` → outbound dial address.
#[derive(Debug, Clone)]
pub struct PeerAddr {
    /// Peer's stable node id (matches the `node_id` field the
    /// peer puts in its `HB`).
    pub node_id: String,
    /// Peer's elect-control host (IP or DNS).
    pub host: String,
    /// Peer's elect-control TCP port.
    pub port: u16,
}

/// Public handle to a running transport. Owns the orchestrator +
/// listener + outbound worker threads. Dropping it signals stop
/// and joins (best-effort within `JOIN_TIMEOUT`).
pub struct Transport {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    shared: Arc<Shared>,
    /// Cloned at construction-time so the kevy-server adapter can
    /// query the live `epoch` / `role` / `current_primary` without
    /// owning the inbound channel.
    state_view: Arc<Shared>,
}

impl Transport {
    /// Spawn the listener, per-peer outbound workers, and the
    /// orchestrator. Returns immediately — the threads run until
    /// `Transport` is dropped.
    ///
    /// `listen_addr` is the local `host:port` the listener binds
    /// to (typically `0.0.0.0:elect_port`). `peers` lists every
    /// OTHER node in the cluster (this node's own id is filtered
    /// out by the elector at run-time).
    pub fn spawn(
        elector: Elector,
        hb_interval: Duration,
        listen_addr: (std::net::IpAddr, u16),
        peers: Vec<PeerAddr>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            elector: Mutex::new(elector),
            out_queues: Mutex::new(std::collections::HashMap::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let (inbound_tx, inbound_rx) = channel::<InboundEvent>();

        let listener = TcpListener::bind(listen_addr)?;
        listener.set_nonblocking(false)?;
        let listener_stop = stop.clone();
        let listener_tx = inbound_tx.clone();
        handles.push(
            std::thread::Builder::new()
                .name("kevy-elect-listener".to_string())
                .spawn(move || {
                    accept_loop(listener, listener_tx, listener_stop);
                })?,
        );

        for peer in &peers {
            let peer_stop = stop.clone();
            let peer_shared = shared.clone();
            let peer_clone = peer.clone();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("kevy-elect-out-{}", peer.node_id))
                    .spawn(move || {
                        outbound_loop(peer_clone, peer_shared, peer_stop);
                    })?,
            );
        }

        let orch_stop = stop.clone();
        let orch_shared = shared.clone();
        handles.push(
            std::thread::Builder::new()
                .name("kevy-elect-orchestrator".to_string())
                .spawn(move || {
                    orchestrator_loop(orch_shared, inbound_rx, hb_interval, orch_stop);
                })?,
        );

        Ok(Self {
            stop,
            handles,
            state_view: shared.clone(),
            shared,
        })
    }

    /// Read-side snapshot of the elector for `ROLE` / `INFO
    /// replication`. Locks the elector mutex briefly; cheap.
    pub fn state_snapshot(&self) -> ElectorSnapshot {
        let e = self.state_view.elector.lock().expect("elector lock");
        ElectorSnapshot {
            role: e.role(),
            epoch: e.epoch(),
            current_primary: e.current_primary().map(str::to_string),
        }
    }

    /// Feed this node's replication offset into the elector.
    pub fn set_repl_offset(&self, offset: u64) {
        self.shared
            .elector
            .lock()
            .expect("elector lock")
            .set_repl_offset(offset);
    }

    /// Stop the transport. Joins all threads (with best-effort
    /// timeout). Idempotent.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Drain handles. We can't tell threads to exit a blocking
        // recv mid-flight (channel close on Sender drop handles it),
        // but the per-loop checks of `stop` flag are the canonical
        // exit signal.
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Read-side snapshot returned by [`Transport::state_snapshot`].
#[derive(Debug, Clone)]
pub struct ElectorSnapshot {
    /// Self-perceived role at snapshot time.
    pub role: crate::message::Role,
    /// Election epoch at snapshot time.
    pub epoch: u64,
    /// Currently-known primary id (`None` until first ANNOUNCE).
    pub current_primary: Option<String>,
}

// ─────────── per-thread loops ───────────

fn accept_loop(listener: TcpListener, tx: Sender<InboundEvent>, stop: Arc<AtomicBool>) {
    // Non-blocking + short sleep so the loop can observe `stop`
    // between accepts. Blocking `accept` would need a Shutdown-on-
    // try_clone trick to interrupt; the non-blocking poll keeps the
    // surface uniform with the outbound loop's busy-but-cheap
    // pattern (election control plane is low-volume).
    listener
        .set_nonblocking(true)
        .expect("listener set_nonblocking(true)");
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let _ = stream.set_nonblocking(false); // children block on reads.
                let tx_clone = tx.clone();
                let stop_clone = stop.clone();
                let addr_str = addr.to_string();
                let _ = std::thread::Builder::new()
                    .name(format!("kevy-elect-in-{addr_str}"))
                    .spawn(move || {
                        inbound_read_loop(stream, addr_str, tx_clone, stop_clone);
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                std::thread::sleep(READ_RETRY_BACKOFF);
            }
        }
    }
}

fn inbound_read_loop(
    mut stream: TcpStream,
    peer_addr: String,
    tx: Sender<InboundEvent>,
    stop: Arc<AtomicBool>,
) {
    let _ = stream.set_nodelay(true);
    // Short read timeout so the loop can observe `stop` between
    // reads. Blocking read otherwise can't be interrupted by a
    // flag.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf: Vec<u8> = Vec::with_capacity(READ_BUF_CAP);
    let mut chunk = [0u8; 1024];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                return;
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > READ_BUF_CAP {
                    let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                    return;
                }
                while !buf.is_empty() {
                    match decode(&buf) {
                        Ok((msg, used)) => {
                            let from = message_sender(&msg);
                            let _ = tx.send(InboundEvent::Message(from, msg));
                            buf.drain(..used);
                        }
                        Err(DecodeError::Truncated) => break,
                        Err(_) => {
                            let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                            return;
                        }
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Read timeout — loop to re-check `stop`.
                continue;
            }
            Err(_) => {
                let _ = tx.send(InboundEvent::InboundConnFailed(peer_addr.clone()));
                return;
            }
        }
    }
}

fn message_sender(msg: &Message) -> String {
    // Every message variant carries the sender's id in a known
    // field — use that as the per-elector "from" key for the
    // orchestrator's on_message route.
    match msg {
        Message::Hb { node_id, .. } => node_id.clone(),
        Message::Offer { candidate_id, .. } => candidate_id.clone(),
        Message::Accept { accepter_id, .. } => accepter_id.clone(),
        Message::Announce { new_primary_id, .. } => new_primary_id.clone(),
    }
}

fn outbound_loop(peer: PeerAddr, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let mut stream: Option<TcpStream> = None;
    while !stop.load(Ordering::Relaxed) {
        if stream.is_none() {
            stream = dial(&peer);
            if stream.is_none() {
                std::thread::sleep(READ_RETRY_BACKOFF);
                continue;
            }
        }
        // Drain this peer's outbound queue.
        let next_msg = {
            let mut qs = shared.out_queues.lock().expect("out_queues lock");
            qs.get_mut(&peer.node_id).and_then(|q| q.pop_front())
        };
        let Some(msg) = next_msg else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        let bytes = encode(&msg);
        let Some(s) = stream.as_mut() else {
            continue;
        };
        if s.write_all(&bytes).is_err() {
            // Connection died. Drop + reconnect next iter; re-
            // queue the in-flight message at the head.
            let _ = s.shutdown(Shutdown::Both);
            stream = None;
            let mut qs = shared.out_queues.lock().expect("out_queues lock");
            qs.entry(peer.node_id.clone()).or_default().push_front(msg);
        }
    }
}

fn dial(peer: &PeerAddr) -> Option<TcpStream> {
    let target = (peer.host.as_str(), peer.port);
    let addr_iter = target.to_socket_addrs().ok()?;
    for sa in addr_iter {
        match TcpStream::connect_timeout(&sa, Duration::from_millis(500)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Some(s);
            }
            Err(_) => continue,
        }
    }
    None
}

fn orchestrator_loop(
    shared: Arc<Shared>,
    inbound_rx: Receiver<InboundEvent>,
    hb_interval: Duration,
    stop: Arc<AtomicBool>,
) {
    // Tick at hb_interval — wait up to that long on the inbound
    // channel; either a message arrives + we process it, or the
    // timeout fires + we run tick.
    while !stop.load(Ordering::Relaxed) {
        let mut outs: Vec<Outbound> = Vec::new();
        match inbound_rx.recv_timeout(hb_interval) {
            Ok(InboundEvent::Message(from, msg)) => {
                let now = Instant::now();
                let mut e = shared.elector.lock().expect("elector lock");
                outs.extend(e.on_message(&from, msg, now));
                outs.extend(e.tick(now));
            }
            Ok(InboundEvent::InboundConnFailed(_)) => {
                // Logged elsewhere; no elector state change here
                // (DOWN detection is driven by the lack of HBs, not
                // by the absence of a TCP socket).
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                let mut e = shared.elector.lock().expect("elector lock");
                outs.extend(e.tick(now));
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !outs.is_empty() {
            let mut qs = shared.out_queues.lock().expect("out_queues lock");
            for out in outs {
                let targets: Vec<String> = if out.to == Outbound::BROADCAST {
                    // Broadcast: enqueue to every peer that has a
                    // queue (which is all of them — pre-seeded at
                    // first outbound to that peer).
                    qs.keys().cloned().collect()
                } else {
                    vec![out.to]
                };
                for target in targets {
                    let q = qs.entry(target).or_default();
                    if q.len() < MAX_PENDING_PER_PEER {
                        q.push_back(out.msg.clone());
                    }
                }
            }
        }
    }
}

