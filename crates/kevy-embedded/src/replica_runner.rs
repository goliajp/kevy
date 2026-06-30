//! Replica runner for embed-as-read-replica.
//!
//! When `Config::replica_upstream = Some(...)` (or constructed via
//! [`crate::Store::open_replica`]), the embed store spawns a single
//! background thread that drives a [`kevy_replicate::replica::ReplicaClient`]
//! against the configured primary: handshake, stream frames, apply each
//! frame into the local shards via [`crate::replay::apply`], and reconnect
//! with exponential backoff on disconnect.
//!
//! Local writes against a replica store are rejected at the public API
//! boundary with `READONLY` (see `crate::store::ensure_writable`). The
//! replication stream is the only writer for a replica's keyspace; the
//! local AOF is force-disabled by `open_replica` so a restart doesn't
//! double-apply (the next handshake resumes from the last applied
//! offset, and on a gap the primary ships a snapshot).
//!
//! v1.20 scope: single-URL upstream = single primary shard. Multi-shard
//! mirroring (N URLs, one runner per shard) is a follow-up.

use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kevy_persist::Argv;
use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

use crate::store::{Shards, lock_write};

/// Handle to the background thread streaming from the primary. Owned
/// by `DropGuard` so the runner outlives the public [`crate::Store`]
/// clones but is joined on the last drop.
pub(crate) struct ReplicaRunner {
    stop: Arc<AtomicBool>,
    /// `try_clone`'d socket handle from the live `ReplicaClient`, used
    /// to interrupt a blocking `next_event` from the drop path —
    /// `shutdown(Both)` on the clone unblocks the reader on the
    /// original (same kernel file description). Refreshed each
    /// reconnect; `None` while between connections.
    sock_clone: Arc<Mutex<Option<TcpStream>>>,
    /// Live upstream `host:port`, refreshed by
    /// [`Self::set_upstream`] when an application observes a kevy-
    /// elect ANNOUNCE (or any other failover signal) and retargets
    /// this replica. The runner reads it at every (re)connect, so a
    /// retarget takes effect on the next reconnect window — typically
    /// `backoff_min` after `set_upstream` triggers a forced drop.
    upstream: Arc<Mutex<String>>,
    /// Triggers a `shutdown(Both)` on `sock_clone` plus an explicit
    /// "skip the rest of this backoff slice" so a retarget reaches
    /// the network within `backoff_min`, not whatever the backoff
    /// ramped to during a prior connect failure.
    force_reconnect: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
    /// Last applied frame offset + 1 (== expected next offset). Read
    /// by `INFO replication` / `kevy_metric::ReplicationLag` follow-ups.
    /// Allow-dead-code until T2.8 e2e wires the reader through the
    /// public `Store` API.
    #[allow(dead_code)]
    pub(crate) applied_offset: Arc<AtomicU64>,
    /// `true` while the runner is connected + post-handshake. Drops to
    /// `false` on disconnect, flips back when the next connect
    /// succeeds. Same dead-code allowance as `applied_offset`.
    #[allow(dead_code)]
    pub(crate) link_up: Arc<AtomicBool>,
}

impl ReplicaRunner {
    /// Spawn the background thread. Never blocks: the first connect
    /// happens on the runner thread, not the caller. Returns
    /// immediately; check [`Self::link_up`] to observe the first
    /// successful handshake.
    pub(crate) fn spawn(
        shards: Shards,
        upstream: String,
        replica_id: String,
        backoff_min: Duration,
        backoff_max: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let sock_clone = Arc::new(Mutex::new(None::<TcpStream>));
        let upstream_slot = Arc::new(Mutex::new(upstream));
        let force_reconnect = Arc::new(AtomicBool::new(false));
        let applied_offset = Arc::new(AtomicU64::new(0));
        let link_up = Arc::new(AtomicBool::new(false));

        let stop_c = Arc::clone(&stop);
        let sock_c = Arc::clone(&sock_clone);
        let upstream_c = Arc::clone(&upstream_slot);
        let force_c = Arc::clone(&force_reconnect);
        let offset_c = Arc::clone(&applied_offset);
        let link_c = Arc::clone(&link_up);

        let join = thread::Builder::new()
            .name("kevy-embedded-replica".into())
            .spawn(move || {
                run_loop(
                    shards,
                    upstream_c,
                    replica_id,
                    stop_c,
                    sock_c,
                    force_c,
                    offset_c,
                    link_c,
                    backoff_min,
                    backoff_max,
                );
            })
            .expect("kevy-embedded: failed to spawn replica runner thread");

        Self {
            stop,
            sock_clone,
            upstream: upstream_slot,
            force_reconnect,
            join: Mutex::new(Some(join)),
            applied_offset,
            link_up,
        }
    }

    /// Retarget this replica at a new primary URL (`host:port`).
    /// Returns immediately; the runner picks up the new upstream on
    /// its next reconnect — which is forced now (within `backoff_min`)
    /// by `shutdown`ing the current socket clone. Idempotent — calling
    /// with the same URL still triggers a force-reconnect, useful when
    /// the operator wants to bounce a flaky link.
    ///
    /// Application code drives this from whatever failover signal it
    /// trusts: `kevy_elect::Transport::state_snapshot().current_primary`,
    /// a config-pushed value, an ops-tool RPC, etc. kevy-embedded
    /// itself stays elect-protocol-agnostic.
    pub(crate) fn set_upstream(&self, new_upstream: String) {
        *self
            .upstream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_upstream;
        // A new primary has its own offset axis — the previous
        // `applied_offset` value is meaningless against it. Reset to 0
        // so the next handshake requests a full backlog walk (or a
        // snapshot ship if the new primary's backlog has rolled past
        // 0). The runner's `SnapshotBegin` handler flushes local state
        // before applying the new snapshot, so stale keys from the
        // prior primary don't bleed through.
        self.applied_offset.store(0, Ordering::Relaxed);
        self.force_reconnect.store(true, Ordering::Relaxed);
        if let Some(s) = self
            .sock_clone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Signal stop + interrupt any in-flight blocking read, then join
    /// the thread. Idempotent. Called from `DropGuard::drop`.
    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self
            .sock_clone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            // shutdown(Both) wakes the blocking read on the runner's
            // own socket (same kernel file description as this clone).
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(j) = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = j.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    shards: Shards,
    upstream: Arc<Mutex<String>>,
    replica_id: String,
    stop: Arc<AtomicBool>,
    sock_clone: Arc<Mutex<Option<TcpStream>>>,
    force_reconnect: Arc<AtomicBool>,
    applied_offset: Arc<AtomicU64>,
    link_up: Arc<AtomicBool>,
    backoff_min: Duration,
    backoff_max: Duration,
) {
    let mut backoff = backoff_min;
    while !stop.load(Ordering::Relaxed) {
        // A `set_upstream` arriving between connect attempts triggers
        // an immediate try with the new URL; clear the flag here so a
        // failed connect re-arms the backoff normally.
        force_reconnect.store(false, Ordering::Relaxed);
        let from_offset = applied_offset.load(Ordering::Relaxed);
        let target = upstream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match ReplicaClient::connect(&target, &replica_id, from_offset) {
            Ok(mut client) => {
                if let Ok(s) = client.socket_handle() {
                    *sock_clone
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(s);
                }
                link_up.store(true, Ordering::Relaxed);
                backoff = backoff_min;

                drain_session(&shards, &mut client, &stop, &applied_offset);

                link_up.store(false, Ordering::Relaxed);
                *sock_clone
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
            Err(_) => {
                // Sleep in small slices so a shutdown signal is acted
                // on within `backoff_min`, even when `backoff` has
                // ramped to the maximum.
                sleep_interruptible(&stop, backoff, backoff_min);
                backoff = (backoff * 2).min(backoff_max);
            }
        }
    }
}

fn drain_session(
    shards: &Shards,
    client: &mut ReplicaClient,
    stop: &Arc<AtomicBool>,
    applied_offset: &Arc<AtomicU64>,
) {
    // Snapshot ingest accumulator. `None` outside a snapshot;
    // `Some(buf)` while between `+SNAPSHOT` and `+SNAPSHOT_END`.
    // Snapshot ship is rare (only when the primary's backlog has
    // rolled past the replica's `from_offset`), so we don't try to
    // stream the chunks into the store as they arrive — collect into
    // memory then `load_snapshot_from` once at the end. v1.20 MVP
    // sizes (mailrs-class) make this trivially affordable.
    let mut snap: Option<Vec<u8>> = None;
    while !stop.load(Ordering::Relaxed) {
        match client.next_event() {
            Some(Ok(ReplicaEvent::Frame(frame))) => {
                if snap.is_some() {
                    // ReplicaClient's state machine already rejects
                    // mid-snapshot live frames as UnexpectedInSnapshot;
                    // a Frame inside a snapshot here is impossible by
                    // construction. Defensive `break` instead of
                    // panicking — the runner just reconnects.
                    break;
                }
                apply_frame(shards, &frame.argv);
                applied_offset.store(client.expected_offset(), Ordering::Relaxed);
            }
            Some(Ok(ReplicaEvent::SnapshotBegin)) => {
                // Snapshot is the new ground truth — flush stale local
                // state so it doesn't double-apply alongside the
                // snapshot's keyspace.
                for shard in shards.iter() {
                    let mut g = lock_write(shard);
                    g.store.flushall();
                }
                snap = Some(Vec::new());
            }
            Some(Ok(ReplicaEvent::SnapshotChunk(bytes))) => {
                if let Some(buf) = snap.as_mut() {
                    buf.extend_from_slice(&bytes);
                }
            }
            Some(Ok(ReplicaEvent::SnapshotEnd { ack_offset })) => {
                if let Some(buf) = snap.take() {
                    if !load_snapshot_into_shard0(shards, &buf) {
                        // Decode error — drop the link; reconnect
                        // either lands in the backlog or triggers
                        // another snapshot ship.
                        break;
                    }
                    applied_offset.store(ack_offset, Ordering::Relaxed);
                }
            }
            Some(Err(_)) | None => break,
        }
    }
}

fn apply_frame(shards: &Shards, argv: &Argv) {
    let n = shards.len();
    let idx = route_shard(argv, n);
    let shard = &shards[idx];
    let mut g = lock_write(shard);
    crate::replay::apply(&mut g.store, argv);
    // No AOF append: the primary's stream is the source of truth, not
    // the replica's local log. Replica AOF is force-disabled by
    // `Store::open_replica`.
}

/// Decode an accumulated snapshot payload into shard 0. v1.20 MVP:
/// single-URL upstream = single primary shard mirror, so the snapshot
/// always loads into shard 0; the multi-shard upstream surface is a
/// follow-up and will route each upstream shard's snapshot to its
/// matching local shard. Returns `false` on decode error (caller drops
/// the link).
fn load_snapshot_into_shard0(shards: &Shards, payload: &[u8]) -> bool {
    let shard = &shards[0];
    let mut g = lock_write(shard);
    let cursor = std::io::Cursor::new(payload);
    kevy_persist::load_snapshot_from(&mut g.store, cursor).is_ok()
}

/// Route a mutation argv to its destination shard. argv[0] is the
/// command, argv[1] is the key for almost every mutation kevy supports
/// (SET k v, DEL k, INCR k, HSET k …, LPUSH k …, ZADD k …). Keyless
/// commands (FLUSHALL, PUBLISH) fall back to shard 0 — same convention
/// `crate::store::lock()` uses for the pub/sub bus.
fn route_shard(argv: &Argv, n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let Some(key) = argv.get(1) else {
        return 0;
    };
    (kevy_hash::key_hash_slot(key) as usize) % n
}

/// Sleep `dur` in slices of `slice`, checking `stop` between slices.
/// Used during the reconnect backoff so a `shutdown()` is acted on
/// within `slice`, not whatever the backoff ramped to.
fn sleep_interruptible(stop: &Arc<AtomicBool>, dur: Duration, slice: Duration) {
    let mut remaining = dur;
    while !stop.load(Ordering::Relaxed) && remaining > Duration::ZERO {
        let chunk = remaining.min(slice);
        thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    #[test]
    fn route_keyless_goes_to_shard_zero() {
        let a = argv(&[b"FLUSHALL"]);
        assert_eq!(route_shard(&a, 4), 0);
    }

    #[test]
    fn route_single_shard_always_zero() {
        let a = argv(&[b"SET", b"any-key", b"v"]);
        assert_eq!(route_shard(&a, 1), 0);
    }

    #[test]
    fn route_keyed_is_deterministic_by_key_hash() {
        let a = argv(&[b"SET", b"k1", b"v"]);
        let b = argv(&[b"DEL", b"k1"]);
        // Same key → same shard regardless of command name.
        assert_eq!(route_shard(&a, 8), route_shard(&b, 8));
    }
}
