//! Per-shard replica runner — the OS thread that holds the outbound
//! TCP link to an upstream primary's per-shard replication port and
//! drives a `kevy_replicate::replica::ReplicaClient`. Each event the
//! client surfaces is forwarded into the matching shard's
//! `ReplicaInboxSender`, where the reactor thread picks
//! it up at the next tick and applies it under
//! `ReplicatedApplyGuard`.
//!
//! Fleet model: one runner per local shard, one upstream port per
//! upstream shard. Multi-shard kevy means the embedder spawns
//! `nshards` runners; runner `i` connects to
//! `(upstream_host, upstream_port_base + i)`.
//!
//! Reconnect: on peer EOF / handshake fail / I/O error the runner
//! sleeps `RECONNECT_BACKOFF` and re-dials, resuming from the
//! highest offset it has seen so far (`from_offset`, advanced by
//! every applied frame or `SnapshotEnd`). The upstream primary's
//! backlog decides whether the resume succeeds (offset still in
//! backlog) or it triggers a fresh snapshot ship.

use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use kevy_replicate::replica::ReplicaClient;
use kevy_rt::ReplicaInboxSender;

use crate::replica_runner_events::drain_client;
use crate::state::ReplicaProgress;

/// Backoff between reconnect attempts when the upstream link drops.
/// Conservative — fast enough that a transient blip recovers within
/// a tick, slow enough that a long-down primary doesn't pin a CPU.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Handle for a per-shard runner thread. The kevy server keeps a
/// `Vec<ReplicaRunner>` in its `ReplicationState` so `REPLICAOF`
/// can stop + replace runners at runtime and so the
/// process exits cleanly via `Drop`.
pub(crate) struct ReplicaRunner {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// `try_clone`'d handle on the current upstream socket — shared
    /// with the runner thread (which updates it on each reconnect)
    /// and the shutdown path (which calls `shutdown(Shutdown::Both)`
    /// to unblock a `next_event` parked in a blocking socket read).
    /// `None` when the runner is between connections (reconnecting).
    socket: Arc<Mutex<Option<TcpStream>>>,
}

impl ReplicaRunner {
    /// Spawn the runner thread. Returns immediately — the thread
    /// connects asynchronously and reconnects on failure until
    /// [`Self::shutdown`] is called.
    /// `runner_slot` indexes this runner's applied-offset slot in
    /// `progress` (= shard id in the fleet model) — the
    /// election-offset sum reads it. `progress` is the
    /// ONLY state slice the runner thread captures.
    pub(crate) fn spawn(
        upstream_addr: (std::net::IpAddr, u16),
        replica_id: String,
        sender: ReplicaInboxSender,
        runner_slot: usize,
        progress: Arc<ReplicaProgress>,
    ) -> Self {
        Self::spawn_target(
            upstream_addr,
            replica_id,
            Target::PerShard(sender),
            runner_slot,
            progress,
        )
    }

    /// Single-source mode: ONE runner drains one upstream
    /// stream and fans events into EVERY shard's inbox (see
    /// [`route_event`]).
    pub(crate) fn spawn_routed(
        upstream_addr: (std::net::IpAddr, u16),
        replica_id: String,
        senders: Vec<ReplicaInboxSender>,
        runner_slot: usize,
        progress: Arc<ReplicaProgress>,
    ) -> Self {
        Self::spawn_target(
            upstream_addr,
            replica_id,
            Target::Routed(senders),
            runner_slot,
            progress,
        )
    }

    fn spawn_target(
        upstream_addr: (std::net::IpAddr, u16),
        replica_id: String,
        target: Target,
        runner_slot: usize,
        progress: Arc<ReplicaProgress>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let socket: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
        let socket_thread = socket.clone();
        let handle = std::thread::Builder::new()
            .name(format!("kevy-replica-{replica_id}"))
            .spawn(move || {
                run_loop(
                    upstream_addr,
                    replica_id,
                    target,
                    stop_thread,
                    socket_thread,
                    runner_slot,
                    progress,
                );
            })
            .expect("spawn replica runner thread");
        Self { handle: Some(handle), stop, socket }
    }

    /// Signal the runner to stop and join the thread. Sets the flag,
    /// then `shutdown(Shutdown::Both)`s the current upstream socket
    /// to break any in-flight blocking `next_event` read. Returns
    /// once the thread joins (within one `RECONNECT_BACKOFF` window
    /// in the worst case — the runner is reconnecting and not in a
    /// blocking read). Called by REPLICAOF retarget / NO ONE.
    #[allow(dead_code)] // wired from REPLICAOF — kept on the API surface
    pub(crate) fn shutdown(mut self) {
        self.signal_stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(guard) = self.socket.lock()
            && let Some(s) = guard.as_ref()
        {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for ReplicaRunner {
    fn drop(&mut self) {
        // Don't drop a still-running thread without signalling — the
        // OS thread holds the TCP fd + a clone of the inbox sender,
        // and may run forever otherwise.
        self.signal_stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Runner body. Connects → loops `next_event` → forwards via sender →
/// reconnect on failure. Tracks `from_offset` to resume after a
/// reconnect within the upstream's backlog window. The `socket_slot`
/// holds the current upstream socket's `try_clone`'d handle so the
/// shutdown path can `Shutdown::Both` it from another thread,
/// unblocking any in-flight blocking read.
/// Where a runner delivers events.
enum Target {
    /// Fleet model: this runner feeds exactly one shard.
    PerShard(ReplicaInboxSender),
    /// Single-source model: one runner feeds every shard.
    Routed(Vec<ReplicaInboxSender>),
}

/// One dial-and-drain attempt. Returns the offset to resume from — the one
/// it reached, or the one it was given when the dial failed.
///
/// Split from `run_loop` for the 50-line rule, at the seam that function
/// already had: the loop above owns the stop flag and the backoff, this
/// owns one connection's lifetime.
#[allow(clippy::too_many_arguments)]
fn one_session(
    upstream_addr: (std::net::IpAddr, u16),
    replica_id: &str,
    target: &Target,
    stop: &Arc<AtomicBool>,
    socket_slot: &Arc<Mutex<Option<TcpStream>>>,
    runner_slot: usize,
    progress: &Arc<ReplicaProgress>,
    from_offset: u64,
    data_gen: &mut u64,
) -> u64 {
    match ReplicaClient::connect_at(
        upstream_addr,
        replica_id,
        *data_gen,
        from_offset,
        Duration::from_secs(5),
    ) {
        Ok(mut client) => {
            drain_session(&mut client, target, stop, socket_slot, runner_slot, progress, data_gen)
        }
        Err(e) => {
            eprintln!(
                "kevy: replica runner '{replica_id}' connect to \
                 {upstream_addr:?} failed: {e}; retrying in \
                 {RECONNECT_BACKOFF:?}"
            );
            from_offset
        }
    }
}

fn run_loop(
    upstream_addr: (std::net::IpAddr, u16),
    replica_id: String,
    target: Target,
    stop: Arc<AtomicBool>,
    socket_slot: Arc<Mutex<Option<TcpStream>>>,
    runner_slot: usize,
    progress: Arc<ReplicaProgress>,
) {
    let mut from_offset: u64 = 0;
    // Feed generation the locally-applied data reflects (0 = nothing
    // applied yet). Presented in the handshake so the primary's
    // generation fence can tell a safe offset resume from an aliasing
    // one; updated whenever this runner adopts a new history (see
    // `drain_client`).
    let mut data_gen: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        from_offset = one_session(
            upstream_addr,
            &replica_id,
            &target,
            &stop,
            &socket_slot,
            runner_slot,
            &progress,
            from_offset,
            &mut data_gen,
        );
        // Reconnect backoff — short enough that a transient blip
        // recovers within a tick, but long enough that a long-down
        // primary doesn't pin a CPU.
        if !stop.load(Ordering::Relaxed) {
            std::thread::sleep(RECONNECT_BACKOFF);
        }
    }
}

/// One connected session: publish the socket clone (so the shutdown
/// path can interrupt the blocking read), drain to disconnect, clear
/// the slot. Returns the offset to resume from.
fn drain_session(
    client: &mut ReplicaClient,
    target: &Target,
    stop: &Arc<AtomicBool>,
    socket_slot: &Mutex<Option<TcpStream>>,
    runner_slot: usize,
    progress: &Arc<ReplicaProgress>,
    data_gen: &mut u64,
) -> u64 {
    set_socket_slot(socket_slot, client.socket_handle().ok());
    crate::replica_trace::trace_session_start(runner_slot, client, *data_gen);
    let from_offset = match target {
        Target::PerShard(sender) => {
            drain_client(client, sender, stop, runner_slot, progress, data_gen)
        }
        Target::Routed(senders) => crate::replica_runner_routed::drain_client_routed(
            client,
            senders,
            stop,
            runner_slot,
            progress,
            data_gen,
        ),
    };
    // Clear the slot — the socket the slot held now owns a
    // half-closed fd (or is going to be shut down).
    set_socket_slot(socket_slot, None);
    from_offset
}

/// Store into the shared socket slot (ignoring a poisoned lock — the
/// slot is best-effort shutdown plumbing).
fn set_socket_slot(slot: &Mutex<Option<TcpStream>>, value: Option<TcpStream>) {
    if let Ok(mut guard) = slot.lock() {
        *guard = value;
    }
}
