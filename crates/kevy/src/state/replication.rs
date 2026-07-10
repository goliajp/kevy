//! Replication-plane state owned by [`RuntimeState`] — the slot
//! `REPLICAOF` reaches into to start / stop / replace runner threads
//! at runtime, without changing already-running `Shard`s.
//!
//! Architecture: [`ReplicationState::new`] always allocates `nshards`
//! replica inbox pairs (regardless of `[replication] role`); the
//! receivers are handed to the runtime once via
//! [`RuntimeState::take_replica_inboxes`] → `Runtime::with_replica_inboxes`
//! and the senders stay here for runners to reach. Each shard's
//! `Shard.replica_inbox` is therefore always installed and always
//! cheap to drain (one `Option::is_some` check costs nothing when
//! empty).
//!
//! Threading: replica runner threads capture only the narrow
//! [`ReplicaProgress`] slice (plus their inbox sender) — never the
//! whole [`RuntimeState`] — so a runner can outlive nothing it
//! doesn't own and the state Arc has no back-edge cycles.
//!
//! [`RuntimeState`]: crate::RuntimeState
//! [`RuntimeState::take_replica_inboxes`]: crate::RuntimeState::take_replica_inboxes

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kevy_rt::{ReplicaInboxReceiver, ReplicaInboxSender};

use super::progress::{ReplicaProgress, epoch_ms};
use crate::replica_runner::ReplicaRunner;

/// Everything the server knows about its replication role: the inbox
/// senders, the live runner fleet, the upstream address, and the
/// availability flags (`-READONLY` / `-QUIESCED` / `-NOREPLICAS` /
/// `-STALE`) that gate client reads and writes.
pub(crate) struct ReplicationState {
    /// One per-shard sender to the matching shard's
    /// [`kevy_rt::ReplicaInboxReceiver`]. Length = `nshards`; index =
    /// shard id. Created with the state, never resized.
    senders: Vec<ReplicaInboxSender>,
    /// The matching receivers, parked here until
    /// [`Self::take_inboxes`] hands them to the runtime (`Some` →
    /// taken exactly once). Still `Some` = no runtime drains the
    /// inboxes, so [`Self::start_runners`] refuses to spawn.
    inboxes: Mutex<Option<Vec<ReplicaInboxReceiver>>>,
    /// Live runner threads. `nshards` entries when a replica role is
    /// active (each connecting to one upstream shard port); empty
    /// otherwise. `REPLICAOF` retarget is "stop_runners + start_runners".
    runners: Mutex<Vec<ReplicaRunner>>,
    /// Current upstream `(host, port_base)` — `None` when not running
    /// as a replica. `ROLE` / `INFO replication` report it.
    upstream: Mutex<Option<(IpAddr, u16)>>,
    /// v3.2 single-source mode (config `--replica-single-source`):
    /// ONE upstream port, one stream, a single routing runner fanning
    /// into every shard inbox.
    single_source: bool,
    /// v3.14 A0 — hot-path role flag: `true` while replica runners are
    /// active. Read every client write via `Commands::write_denied`,
    /// so it's an atomic, not the upstream mutex.
    is_replica: AtomicBool,
    /// `replica-read-only` config (default ON, Redis-compatible).
    read_only: AtomicBool,
    /// v3.14 D5 — `min-replicas-to-write` (0 = off).
    min_replicas: AtomicU32,
    /// v3.15 D3 — planned-failover quiesce: while `Some(target)`,
    /// every client write answers `-QUIESCED migrating to <target>`
    /// (the cluster-rw client retries with backoff and follows).
    /// Cleared on completion or abort.
    quiesce_to: Mutex<Option<String>>,
    /// Hot-path flag mirroring `quiesce_to.is_some()` (the mutex is
    /// only taken to render the error text on the cold rejected path).
    quiesced: AtomicBool,
    /// v3.16 D4 — primary quorum lease fence.
    quorum_fenced: AtomicBool,
    /// v3.16 D3 — bounded staleness (0 = off). Set from config at
    /// boot; CONFIG SET updates it live (the operator escape hatch).
    max_staleness_ms: AtomicU64,
    /// v3.16 D2 — promotion counter. Bumped on every replica → primary
    /// transition ([`Self::promote_stop_runners`]); each shard observes
    /// it through `Commands::live_runtime_config` and bumps its feed
    /// generation, fencing the pre-failover offset space against stale
    /// REPL.TOKENs. Never reset — shards latch the first value they
    /// see and only act on increases.
    promotion_epoch: AtomicU64,
    /// The narrow heartbeat/offset slice runner threads write into.
    progress: Arc<ReplicaProgress>,
}

impl ReplicationState {
    /// Build the state for an `nshards`-shard runtime, allocating the
    /// per-shard inbox pairs.
    pub(crate) fn new(nshards: usize, single_source: bool) -> Self {
        let mut senders = Vec::with_capacity(nshards);
        let mut receivers = Vec::with_capacity(nshards);
        for _ in 0..nshards {
            let (tx, rx) = kevy_rt::replica_inbox_pair();
            senders.push(tx);
            receivers.push(rx);
        }
        Self {
            senders,
            inboxes: Mutex::new(Some(receivers)),
            runners: Mutex::new(Vec::new()),
            upstream: Mutex::new(None),
            single_source,
            is_replica: AtomicBool::new(false),
            read_only: AtomicBool::new(true),
            min_replicas: AtomicU32::new(0),
            quiesce_to: Mutex::new(None),
            quiesced: AtomicBool::new(false),
            quorum_fenced: AtomicBool::new(false),
            max_staleness_ms: AtomicU64::new(0),
            promotion_epoch: AtomicU64::new(0),
            progress: Arc::new(ReplicaProgress::default()),
        }
    }

    /// Hand out the per-shard inbox receivers (once). See
    /// [`crate::RuntimeState::take_replica_inboxes`].
    pub(crate) fn take_inboxes(&self) -> Option<Vec<ReplicaInboxReceiver>> {
        self.inboxes.lock().expect("inboxes poisoned").take()
    }

    /// Stop every active runner thread, joining each. After this
    /// returns, [`Self::is_replica`] is `false` and the upstream slot
    /// is `None`. Called by `REPLICAOF NO ONE`, by retarget (before
    /// [`Self::start_runners`] puts the new ones), and on shutdown.
    pub(crate) fn stop_runners(&self) {
        self.is_replica.store(false, Ordering::Relaxed);
        let mut guard = self.runners.lock().expect("runners poisoned");
        let runners = std::mem::take(&mut *guard);
        drop(guard); // release the lock before potentially-blocking joins
        for r in runners {
            r.shutdown();
        }
        *self.upstream.lock().expect("upstream poisoned") = None;
        self.progress.clear_runner_slots();
    }

    /// Replace the active runner set with a fresh fleet pointing at
    /// `(upstream_host, upstream_port_base)`. Each shard `i` gets a
    /// runner connecting to `(upstream_host, upstream_port_base + i)`.
    /// Idempotent w.r.t. previous fleet — any existing runners are
    /// shut down before the new fleet spawns.
    ///
    /// Returns `Err` only when the inbox receivers were never wired
    /// into a runtime (embedded `dispatch` without `kevy::serve` /
    /// `Runtime::with_replica_inboxes`) — every other failure mode is
    /// the runner thread's reconnect loop handling transient upstream
    /// unreachability.
    pub(crate) fn start_runners(&self, upstream: (IpAddr, u16)) -> Result<(), &'static str> {
        if self.inboxes.lock().expect("inboxes poisoned").is_some() {
            return Err("replica inboxes not wired into a runtime (kevy::serve required)");
        }
        // Stop any prior fleet before installing the new one. The old
        // runners' threads block on `next_event` reads; shutdown()
        // shuts down their sockets so the reads unblock and join
        // completes within ~one event.
        self.stop_runners();
        let (host, port_base) = upstream;
        // Size the per-runner applied-offset registry BEFORE spawning —
        // a runner's first heartbeat may land before this function
        // returns, and its slot must already exist.
        let runner_count = if self.single_source { 1 } else { self.senders.len() };
        self.progress.size_runner_slots(runner_count);
        let new_runners = if self.single_source {
            vec![ReplicaRunner::spawn_routed(
                (host, port_base),
                "kevy-replica-single".to_string(),
                self.senders.clone(),
                0,
                Arc::clone(&self.progress),
            )]
        } else {
            let mut fleet = Vec::with_capacity(self.senders.len());
            for (shard_id, sender) in self.senders.iter().enumerate() {
                let port = port_base.saturating_add(u16::try_from(shard_id).unwrap_or(u16::MAX));
                let replica_id = format!("kevy-replica-{shard_id}");
                fleet.push(ReplicaRunner::spawn(
                    (host, port),
                    replica_id,
                    sender.clone(),
                    shard_id,
                    Arc::clone(&self.progress),
                ));
            }
            fleet
        };
        *self.runners.lock().expect("runners poisoned") = new_runners;
        *self.upstream.lock().expect("upstream poisoned") = Some(upstream);
        // AFTER the internal stop_runners above (which clears the
        // flag) — the role flips to replica only once the new fleet
        // is installed.
        self.is_replica.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Stop runners as part of a PROMOTION (`REPLICAOF NO ONE` on a
    /// following replica, or an election win). When this node really
    /// was a replica, the promotion counter bumps so every shard
    /// fences its offset space (feed generation bump — see
    /// `kevy_rt::LiveRuntimeConfig::promotion_epoch`). A plain
    /// [`Self::stop_runners`] (retarget teardown, shutdown) never bumps.
    pub(crate) fn promote_stop_runners(&self) {
        let was_replica = self.is_replica();
        self.stop_runners();
        if was_replica {
            self.promotion_epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn is_replica(&self) -> bool {
        self.is_replica.load(Ordering::Relaxed)
    }

    /// v3.15 restart-role clamp: mark this node a replica WITHOUT any
    /// runners — a restarted quorum member holds writes until an
    /// election outcome flips the flag (win → stop_runners clears it).
    pub(crate) fn force_replica_flag(&self) {
        self.is_replica.store(true, Ordering::Relaxed);
    }

    pub(crate) fn set_read_only(&self, on: bool) {
        self.read_only.store(on, Ordering::Relaxed);
    }

    pub(crate) fn read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed)
    }

    pub(crate) fn set_min_replicas(&self, n: u32) {
        self.min_replicas.store(n, Ordering::Relaxed);
    }

    pub(crate) fn set_max_staleness_ms(&self, v: u64) {
        self.max_staleness_ms.store(v, Ordering::Relaxed);
    }

    pub(crate) fn set_quiesce(&self, target: Option<String>) {
        let active = target.is_some();
        *self.quiesce_to.lock().expect("quiesce_to poisoned") = target;
        self.quiesced.store(active, Ordering::Relaxed);
    }

    pub(crate) fn quiesce_active(&self) -> bool {
        self.quiesced.load(Ordering::Relaxed)
    }

    /// v3.16 D4 — flip the quorum lease fence. Returns whether the
    /// flag CHANGED (callers log transitions only).
    pub(crate) fn set_quorum_fence(&self, on: bool) -> bool {
        self.quorum_fenced.swap(on, Ordering::Relaxed) != on
    }

    pub(crate) fn promotion_epoch(&self) -> u64 {
        self.promotion_epoch.load(Ordering::Relaxed)
    }

    /// Read the current upstream — `(host, port_base)` when running as
    /// a replica, `None` otherwise. Used by `ROLE` / `INFO replication`
    /// to report the live (not startup-config) upstream.
    pub(crate) fn current_upstream(&self) -> Option<(IpAddr, u16)> {
        *self.upstream.lock().expect("upstream poisoned")
    }

    /// Snapshot of every runner's last-seen upstream generation, in
    /// runner-slot (= shard, fleet model) order. Empty when no runners.
    pub(crate) fn upstream_gens(&self) -> Vec<u64> {
        self.progress.upstream_gens()
    }

    /// Snapshot of every runner's applied stream position, in
    /// runner-slot order. REPL.TOKEN on a replica reports these — a
    /// "how far this replica is" token.
    pub(crate) fn applied_runner_offsets(&self) -> Vec<u64> {
        self.progress.runner_offsets()
    }

    /// Sum of every runner's applied stream position — the
    /// replica-side election offset (v3.15 D1). Comparable with the
    /// primary side's per-shard `master_repl_offset` sum: both count
    /// "replication-stream position, totalled across streams", and on
    /// a fully-caught-up replica the two sums are equal. 0 when no
    /// runners are active.
    pub(crate) fn applied_offset_sum(&self) -> u64 {
        self.progress.applied_offset_sum()
    }

    /// INFO replication: `(link_up, applied_offset, lag_frames,
    /// last_io_secs)`. Link is up when a heartbeat landed within the
    /// last 3s.
    pub(crate) fn replica_link_view(&self) -> (bool, u64, u64, u64) {
        self.progress.link_view()
    }

    /// Refuse reads on a replica whose primary heartbeat is older than
    /// the staleness bound. Primaries and un-bounded replicas never
    /// refuse (the common case is two relaxed atomic loads).
    pub(crate) fn read_denied_reply(&self) -> Option<Vec<u8>> {
        let bound = self.max_staleness_ms.load(Ordering::Relaxed);
        if bound == 0 || !self.is_replica() {
            return None;
        }
        let last = self.progress.last_ping_ms();
        if last != 0 && epoch_ms().saturating_sub(last) <= bound {
            return None;
        }
        Some(
            b"-STALE replica is stale; read the primary or raise replica_max_staleness_ms\r\n"
                .to_vec(),
        )
    }

    /// The write-availability gate, in fence-strength order: quorum
    /// fence → quiesce → replica read-only → min-replicas. `None` =
    /// the write may proceed.
    pub(crate) fn write_denied_reply(&self) -> Option<Vec<u8>> {
        if self.quorum_fenced.load(Ordering::Relaxed) {
            return Some(b"-NOREPLICAS primary lost quorum; writes fenced\r\n".to_vec());
        }
        if self.quiesced.load(Ordering::Relaxed) {
            let g = self.quiesce_to.lock().expect("quiesce_to poisoned");
            if let Some(t) = g.as_ref() {
                return Some(format!("-QUIESCED migrating to {t}\r\n").into_bytes());
            }
        }
        if self.is_replica() {
            if self.read_only() {
                return Some(
                    b"-READONLY You can't write against a read only replica.\r\n".to_vec(),
                );
            }
            return None;
        }
        let min = self.min_replicas.load(Ordering::Relaxed);
        if min > 0 && crate::ops::replication::healthy_replica_count() < min as usize {
            return Some(b"-NOREPLICAS Not enough good replicas to write.\r\n".to_vec());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_defaults() {
        let r = ReplicationState::new(1, false);
        assert!(!r.is_replica());
        assert!(r.read_only());
        assert!(r.current_upstream().is_none());
        assert_eq!(r.applied_offset_sum(), 0);
        assert!(r.write_denied_reply().is_none());
        assert!(r.read_denied_reply().is_none());
    }

    #[test]
    fn applied_offset_sum_is_per_runner_sum_not_max() {
        let r = ReplicationState::new(3, false);
        assert_eq!(r.applied_offset_sum(), 0, "no runners → 0");
        // Simulate a 3-runner fleet's registry (start_runners sizes
        // this in production).
        r.progress.size_runner_slots(3);
        r.progress.record_ping(0, 1, 100, 40);
        r.progress.record_applied(1, 25);
        r.progress.record_applied(2, 35);
        assert_eq!(r.applied_offset_sum(), 100, "sum across runners, not max");
        // Plain store semantics: a resync rewind must show through.
        r.progress.record_applied(1, 5);
        assert_eq!(r.applied_offset_sum(), 80);
        // Out-of-range slot is ignored (registry resize race guard).
        r.progress.record_applied(9, 1_000);
        assert_eq!(r.applied_offset_sum(), 80);
        // stop_runners clears the registry.
        r.stop_runners();
        assert_eq!(r.applied_offset_sum(), 0);
    }

    #[test]
    fn start_runners_before_runtime_wiring_errors() {
        let r = ReplicationState::new(1, false);
        let result = r.start_runners((IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 6400));
        assert!(result.is_err(), "inboxes never taken → no runtime drains them");
        assert!(!r.is_replica());
    }

    #[test]
    fn promote_bumps_epoch_only_from_replica() {
        let r = ReplicationState::new(1, false);
        r.promote_stop_runners();
        assert_eq!(r.promotion_epoch(), 0, "primary → primary is not a promotion");
        r.force_replica_flag();
        r.promote_stop_runners();
        assert_eq!(r.promotion_epoch(), 1);
        assert!(!r.is_replica());
    }

    #[test]
    fn write_denied_orders_fences() {
        let r = ReplicationState::new(1, false);
        r.set_quiesce(Some("10.0.0.9:7000".into()));
        let reply = r.write_denied_reply().expect("quiesced");
        assert!(reply.starts_with(b"-QUIESCED"), "{}", String::from_utf8_lossy(&reply));
        assert!(r.set_quorum_fence(true), "flag changed");
        let reply = r.write_denied_reply().expect("fenced");
        assert!(reply.starts_with(b"-NOREPLICAS"), "fence outranks quiesce");
        assert!(!r.set_quorum_fence(true), "unchanged flag reports false");
        r.set_quorum_fence(false);
        r.set_quiesce(None);
        r.force_replica_flag();
        let reply = r.write_denied_reply().expect("read-only replica");
        assert!(reply.starts_with(b"-READONLY"));
        r.set_read_only(false);
        assert!(r.write_denied_reply().is_none(), "writable replica");
    }
}
