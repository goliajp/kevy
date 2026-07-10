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

use std::net::TcpListener;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::elector::Elector;
use crate::message::Message;
use crate::transport_loops::{accept_loop, orchestrator_loop, outbound_loop};

/// Maximum buffer the per-connection reader holds before declaring
/// the framing busted. Election frames are ≤ 256 B; 16 KiB is
/// generous for misaligned partial reads.
pub(crate) const READ_BUF_CAP: usize = 16 * 1024;

/// Read-loop sleep on transient EAGAIN-equivalents (peer closed,
/// I/O error during decode). Keeps the worker from a tight retry
/// loop while still recovering on reconnect.
pub(crate) const READ_RETRY_BACKOFF: Duration = Duration::from_millis(100);

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
pub(crate) struct Shared {
    pub(crate) elector: Mutex<Elector>,
    /// Per-peer outbound queue. Indexed by `node_id`. Each worker
    /// drains its own queue + writes onto the persistent TCP
    /// stream; on stream death the queue is held until the worker
    /// reconnects. Bounded by `MAX_PENDING_PER_PEER` to prevent a
    /// dead peer from leaking memory.
    pub(crate) out_queues:
        Mutex<std::collections::HashMap<String, std::collections::VecDeque<Message>>>,
}

/// v3.15 D2 / v3.16 D4 — topology-change callback:
/// `(new_local_role, Some(primary_id) when known, has_quorum)`.
/// Address mapping is the CALLER's job (the static member table
/// lives in the host's config — membership is static, roles are
/// dynamic). `has_quorum` drives the primary lease: a primary seeing
/// `false` is on the minority side of a partition and must fence
/// writes within the `down_after` window.
pub type TopologyCallback = Box<dyn Fn(crate::message::Role, Option<String>, bool) + Send>;

pub(crate) const MAX_PENDING_PER_PEER: usize = 256;

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
        Self::spawn_with_callback(elector, hb_interval, listen_addr, peers, Box::new(|_, _, _| {}))
    }

    /// v3.15 D2 — like [`Self::spawn`], with a topology-change
    /// callback: fired from the orchestrator thread whenever
    /// `(role, current_primary)` changes after a message or tick.
    /// Arguments: the new local role, and `Some((primary_id,
    /// primary_addr))` when a primary is known. The callback MUST be
    /// quick and non-reentrant into the elector (it runs outside the
    /// elector lock but on the tick thread).
    // needless_pass_by_value: `peers` is handed to the spawned outbound loops
    // one entry at a time; by-value keeps the pub API an ownership handoff.
    #[allow(clippy::needless_pass_by_value)]
    pub fn spawn_with_callback(
        elector: Elector,
        hb_interval: Duration,
        listen_addr: (std::net::IpAddr, u16),
        peers: Vec<PeerAddr>,
        on_change: TopologyCallback,
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
        spawn_listener_thread(listener, inbound_tx.clone(), stop.clone(), &mut handles)?;
        spawn_outbound_threads(&peers, &shared, &stop, &mut handles)?;

        let orch_stop = stop.clone();
        let orch_shared = shared.clone();
        handles.push(
            std::thread::Builder::new()
                .name("kevy-elect-orchestrator".to_string())
                .spawn(move || {
                    orchestrator_loop(orch_shared, inbound_rx, hb_interval, orch_stop, on_change);
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
    // missing_panics_doc: lock().expect — poisoning means another thread
    // already panicked mid-election; propagating is the only sane behaviour.
    #[allow(clippy::missing_panics_doc)]
    pub fn state_snapshot(&self) -> ElectorSnapshot {
        let e = self.state_view.elector.lock().expect("elector lock");
        let now = std::time::Instant::now();
        // T3.11 / F4: include the list of peers this node considers
        // DOWN at snapshot time. kevy-scope's F4 fallback path reads
        // this to decide "writer DOWN → fallback takes over"; the
        // computation here is cheap (one pass over peer_ids).
        let down_peers: Vec<String> = e
            .peer_ids
            .iter()
            .filter(|id| id.as_str() != e.node_id.as_str())
            .filter(|id| e.is_peer_down(id, now))
            .cloned()
            .collect();
        ElectorSnapshot {
            role: e.role(),
            epoch: e.epoch(),
            current_primary: e.current_primary().map(str::to_string),
            down_peers,
        }
    }

    /// Feed this node's replication offset into the elector.
    // missing_panics_doc: same poisoned-lock rationale as `state_snapshot`.
    #[allow(clippy::missing_panics_doc)]
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
    /// Peers (excluding self) whose last `HB` is older than
    /// `ElectConfig::down_after` — i.e. the down-set this node would
    /// vote on at quorum time. kevy-scope's F4 fallback reads this
    /// to decide whether the declared scope writer is reachable;
    /// when the writer's id is present, the fallback takes over the
    /// scope's writes.
    pub down_peers: Vec<String>,
}

// ─────────── per-thread loops ───────────

/// Spawn the accept-side listener thread, appending its handle.
fn spawn_listener_thread(
    listener: TcpListener,
    tx: Sender<InboundEvent>,
    stop: Arc<AtomicBool>,
    handles: &mut Vec<JoinHandle<()>>,
) -> std::io::Result<()> {
    handles.push(
        std::thread::Builder::new()
            .name("kevy-elect-listener".to_string())
            .spawn(move || {
                accept_loop(listener, tx, stop);
            })?,
    );
    Ok(())
}

/// Spawn one outbound worker thread per peer, appending the handles.
fn spawn_outbound_threads(
    peers: &[PeerAddr],
    shared: &Arc<Shared>,
    stop: &Arc<AtomicBool>,
    handles: &mut Vec<JoinHandle<()>>,
) -> std::io::Result<()> {
    for peer in peers {
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
    Ok(())
}
