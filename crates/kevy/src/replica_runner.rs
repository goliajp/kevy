//! Per-shard replica runner — the OS thread that holds the outbound
//! TCP link to an upstream primary's per-shard replication port and
//! drives a `kevy_replicate::replica::ReplicaClient`. Each event the
//! client surfaces is forwarded into the matching shard's
//! `ReplicaInboxSender` (T1.29(c)), where the reactor thread picks
//! it up at the next tick and applies it under
//! `ReplicatedApplyGuard`.
//!
//! v1.18 model: one runner per local shard, one upstream port per
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
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
use kevy_rt::{ReplicaApply, ReplicaInboxSender};

/// Backoff between reconnect attempts when the upstream link drops.
/// Conservative — fast enough that a transient blip recovers within
/// a tick, slow enough that a long-down primary doesn't pin a CPU.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Handle for a per-shard runner thread. The kevy server keeps a
/// `Vec<ReplicaRunner>` in a process-global slot (`REPLICA_RUNNERS`)
/// so `REPLICAOF` (T1.29.5) can stop + replace runners at runtime
/// and so the process exits cleanly via `Drop`.
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
    pub(crate) fn spawn(
        upstream_addr: (std::net::IpAddr, u16),
        replica_id: String,
        sender: ReplicaInboxSender,
    ) -> Self {
        Self::spawn_target(upstream_addr, replica_id, Target::PerShard(sender))
    }

    /// v3.2 — single-source mode: ONE runner drains one upstream
    /// stream and fans events into EVERY shard's inbox (see
    /// [`route_event`]).
    pub(crate) fn spawn_routed(
        upstream_addr: (std::net::IpAddr, u16),
        replica_id: String,
        senders: Vec<ReplicaInboxSender>,
    ) -> Self {
        Self::spawn_target(upstream_addr, replica_id, Target::Routed(senders))
    }

    fn spawn_target(
        upstream_addr: (std::net::IpAddr, u16),
        replica_id: String,
        target: Target,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let socket: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
        let socket_thread = socket.clone();
        let handle = std::thread::Builder::new()
            .name(format!("kevy-replica-{replica_id}"))
            .spawn(move || {
                run_loop(upstream_addr, replica_id, target, stop_thread, socket_thread);
            })
            .expect("spawn replica runner thread");
        Self {
            handle: Some(handle),
            stop,
            socket,
        }
    }

    /// Signal the runner to stop and join the thread. Sets the flag,
    /// then `shutdown(Shutdown::Both)`s the current upstream socket
    /// to break any in-flight blocking `next_event` read. Returns
    /// once the thread joins (within one `RECONNECT_BACKOFF` window
    /// in the worst case — the runner is reconnecting and not in a
    /// blocking read). Called by REPLICAOF retarget / NO ONE
    /// (T1.29.5 / T1.30).
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
    /// v1.18 fleet model: this runner feeds exactly one shard.
    PerShard(ReplicaInboxSender),
    /// v3.2 single-source model: one runner feeds every shard.
    Routed(Vec<ReplicaInboxSender>),
}

fn run_loop(
    upstream_addr: (std::net::IpAddr, u16),
    replica_id: String,
    target: Target,
    stop: Arc<AtomicBool>,
    socket_slot: Arc<Mutex<Option<TcpStream>>>,
) {
    let mut from_offset: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        match ReplicaClient::connect(upstream_addr, &replica_id, from_offset) {
            Ok(mut client) => {
                // Publish the socket clone so the shutdown path can
                // interrupt the blocking read.
                if let Ok(handle) = client.socket_handle()
                    && let Ok(mut guard) = socket_slot.lock()
                {
                    *guard = Some(handle);
                }
                from_offset = match &target {
                    Target::PerShard(sender) => drain_client(&mut client, sender, &stop),
                    Target::Routed(senders) => drain_client_routed(&mut client, senders, &stop),
                };
                // Clear the slot — the socket the slot held now owns
                // a half-closed fd (or is going to be shut down).
                if let Ok(mut guard) = socket_slot.lock() {
                    *guard = None;
                }
            }
            Err(e) => {
                eprintln!(
                    "kevy: replica runner '{replica_id}' connect to \
                     {upstream_addr:?} failed: {e}; retrying in \
                     {RECONNECT_BACKOFF:?}"
                );
            }
        }
        // Reconnect backoff — short enough that a transient blip
        // recovers within a tick, but long enough that a long-down
        // primary doesn't pin a CPU.
        if !stop.load(Ordering::Relaxed) {
            std::thread::sleep(RECONNECT_BACKOFF);
        }
    }
}

/// Drain `next_event` until the peer EOFs / errors. Returns the
/// `from_offset` to resume from on the next reconnect.
fn drain_client(
    client: &mut ReplicaClient,
    sender: &ReplicaInboxSender,
    stop: &Arc<AtomicBool>,
) -> u64 {
    let mut from_offset = client.expected_offset();
    while !stop.load(Ordering::Relaxed) {
        match client.next_event() {
            Some(Ok(event)) => {
                let apply = event_to_apply(event, &mut from_offset);
                if sender.send(apply).is_err() {
                    // Receiver dropped — the shard / runtime is gone;
                    // the runner should also exit.
                    return from_offset;
                }
            }
            Some(Err(e)) => {
                eprintln!("kevy: replica runner upstream error: {e}");
                return from_offset;
            }
            None => return from_offset, // clean peer EOF — reconnect
        }
    }
    from_offset
}

fn event_to_apply(event: ReplicaEvent, from_offset: &mut u64) -> ReplicaApply {
    match event {
        ReplicaEvent::SnapshotBegin => ReplicaApply::SnapshotBegin,
        ReplicaEvent::SnapshotChunk(bytes) => ReplicaApply::SnapshotChunk(bytes),
        ReplicaEvent::SnapshotEnd { ack_offset } => {
            *from_offset = ack_offset;
            ReplicaApply::SnapshotEnd { ack_offset, routed: false }
        }
        ReplicaEvent::Frame(frame) => {
            *from_offset = frame.offset.saturating_add(1);
            ReplicaApply::Frame {
                offset: frame.offset,
                argv: frame.argv,
            }
        }
    }
}

// ---------- v3.2 single-source (embedded-as-primary) mode ----------

/// Route one event fan into N shard inboxes: snapshot control/chunks
/// BROADCAST (each shard loads its own hash slice — SnapshotEnd
/// carries `routed: true`); keyed frames route by hash slot; the
/// keyless flushes broadcast; other keyless frames go to shard 0
/// (pub/sub convention).
fn route_event(
    event: ReplicaEvent,
    from_offset: &mut u64,
    senders: &[ReplicaInboxSender],
) -> Result<(), ()> {
    let n = senders.len();
    let send_all = |apply: &dyn Fn() -> ReplicaApply| -> Result<(), ()> {
        for s in senders {
            s.send(apply()).map_err(|_| ())?;
        }
        Ok(())
    };
    match event {
        ReplicaEvent::SnapshotBegin => send_all(&|| ReplicaApply::SnapshotBegin),
        ReplicaEvent::SnapshotChunk(bytes) => {
            send_all(&|| ReplicaApply::SnapshotChunk(bytes.clone()))
        }
        ReplicaEvent::SnapshotEnd { ack_offset } => {
            *from_offset = ack_offset;
            send_all(&|| ReplicaApply::SnapshotEnd { ack_offset, routed: true })
        }
        ReplicaEvent::Frame(frame) => {
            *from_offset = frame.offset.saturating_add(1);
            let verb = frame.argv.get(0).unwrap_or_default();
            if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
                return send_all(&|| ReplicaApply::Frame {
                    offset: frame.offset,
                    argv: frame.argv.clone(),
                });
            }
            let slot = match frame.argv.get(1) {
                Some(key) => (kevy_hash::key_hash_slot(key) as usize) % n,
                None => 0,
            };
            senders[slot]
                .send(ReplicaApply::Frame { offset: frame.offset, argv: frame.argv })
                .map_err(|_| ())
        }
    }
}

/// v3.2: drain loop for single-source mode (one upstream conn, all
/// shard inboxes).
fn drain_client_routed(
    client: &mut ReplicaClient,
    senders: &[ReplicaInboxSender],
    stop: &Arc<AtomicBool>,
) -> u64 {
    let mut from_offset = client.expected_offset();
    while !stop.load(Ordering::Relaxed) {
        match client.next_event() {
            Some(Ok(event)) => {
                if route_event(event, &mut from_offset, senders).is_err() {
                    return from_offset;
                }
            }
            Some(Err(e)) => {
                eprintln!("kevy: replica runner upstream error: {e}");
                return from_offset;
            }
            None => return from_offset,
        }
    }
    from_offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_to_apply_snapshot_begin_passthrough() {
        let mut off = 7;
        let out = event_to_apply(ReplicaEvent::SnapshotBegin, &mut off);
        assert!(matches!(out, ReplicaApply::SnapshotBegin));
        assert_eq!(off, 7, "SnapshotBegin must not touch the offset");
    }

    #[test]
    fn event_to_apply_snapshot_end_advances_offset() {
        let mut off = 0;
        let out = event_to_apply(ReplicaEvent::SnapshotEnd { ack_offset: 42 }, &mut off);
        match out {
            ReplicaApply::SnapshotEnd { ack_offset, .. } => assert_eq!(ack_offset, 42),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(off, 42, "SnapshotEnd must jump from_offset to ack_offset");
    }

    #[test]
    fn event_to_apply_frame_advances_offset_by_one() {
        let mut off = 3;
        let frame = kevy_replicate::replica::DecodedFrame {
            offset: 9,
            argv: kevy_rt::Argv::default(),
        };
        let out = event_to_apply(ReplicaEvent::Frame(frame), &mut off);
        assert!(matches!(out, ReplicaApply::Frame { offset: 9, .. }));
        assert_eq!(off, 10, "Frame must advance to offset + 1");
    }
}
