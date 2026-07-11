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

use kevy_resp::CmdError;
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
    /// Serializes whole fleet transitions (stop / start / promote).
    /// The `runners` mutex protects the Vec, but a transition is a
    /// multi-step sequence (flag flip, joins, slot resize, spawn,
    /// upstream label) — two concurrent REPLICAOF / elect callbacks
    /// interleaving those steps could leave the `upstream` label
    /// pointing away from the actual fleet, or double-bump the
    /// promotion epoch. Held across each transition; never taken by
    /// runner threads (they capture only `progress`), so joining
    /// under it cannot deadlock.
    retarget: Mutex<()>,
    /// Current upstream `(host, port_base)` — `None` when not running
    /// as a replica. `ROLE` / `INFO replication` report it.
    upstream: Mutex<Option<(IpAddr, u16)>>,
    /// Single-source mode (config `--replica-single-source`):
    /// ONE upstream port, one stream, a single routing runner fanning
    /// into every shard inbox.
    single_source: bool,
    /// Hot-path role flag: `true` while replica runners are
    /// active. Read every client write via `Commands::write_denied`,
    /// so it's an atomic, not the upstream mutex.
    is_replica: AtomicBool,
    /// `replica-read-only` config (default ON, Redis-compatible).
    read_only: AtomicBool,
    /// `min-replicas-to-write` (0 = off).
    min_replicas: AtomicU32,
    /// Planned-failover quiesce: while `Some(target)`,
    /// every client write answers `-QUIESCED migrating to <target>`
    /// (the cluster-rw client retries with backoff and follows).
    /// Cleared on completion or abort. The hot path never takes this
    /// mutex — the per-shard gate bits answer "possibly
    /// quiesced"; only the already-gated slow path locks it to render
    /// the error text.
    quiesce_to: Mutex<Option<String>>,
    /// Primary quorum lease fence.
    quorum_fenced: AtomicBool,
    /// Bounded staleness (0 = off). Set from config at boot;
    /// replication keys are deliberately absent from the CONFIG SET
    /// hot-set matrix, so this never changes at runtime (an atomic
    /// only because the gate-bits rebuild reads it cross-thread).
    max_staleness_ms: AtomicU64,
    /// Promotion counter. Bumped on every replica → primary
    /// transition ([`Self::promote_stop_runners`]); each shard observes
    /// it through `Commands::live_runtime_config` and bumps its feed
    /// generation, fencing the pre-failover offset space against stale
    /// REPL.TOKENs. Never reset — shards latch the first value they
    /// see and only act on increases.
    promotion_epoch: AtomicU64,
    /// The narrow heartbeat/offset slice runner threads write into.
    progress: Arc<ReplicaProgress>,
    /// This server's advertised client port, baked into the runner
    /// replica-ids (`kevy-replica-<port>#<stream>`) so a primary can
    /// tell its replicas apart and report where to reach them. Set
    /// once at state construction from the config.
    self_port: AtomicU32,
    /// The instance-wide gate invalidation counter, shared
    /// with [`RuntimeState`](crate::RuntimeState) (`Arc` because the
    /// election callback and the FAILOVER thread hold only this
    /// narrow slice, yet their role flips must invalidate every
    /// shard's cached gate bits). Every setter that changes a
    /// write/read-gating authority field bumps it (two-step writer
    /// protocol: write the field, then `fetch_add(1, Release)`).
    control_epoch: Arc<AtomicU64>,
}

impl ReplicationState {
    /// Build the state for an `nshards`-shard runtime, allocating the
    /// per-shard inbox pairs.
    pub(crate) fn new(nshards: usize, single_source: bool, self_port: u16) -> Self {
        let mut senders = Vec::with_capacity(nshards);
        let mut receivers = Vec::with_capacity(nshards);
        for _ in 0..nshards {
            let (tx, rx) = kevy_rt::replica_inbox_pair();
            senders.push(tx);
            receivers.push(rx);
        }
        let control_epoch = Arc::new(AtomicU64::new(0));
        Self {
            senders,
            inboxes: Mutex::new(Some(receivers)),
            runners: Mutex::new(Vec::new()),
            retarget: Mutex::new(()),
            upstream: Mutex::new(None),
            single_source,
            is_replica: AtomicBool::new(false),
            read_only: AtomicBool::new(true),
            min_replicas: AtomicU32::new(0),
            quiesce_to: Mutex::new(None),
            quorum_fenced: AtomicBool::new(false),
            max_staleness_ms: AtomicU64::new(0),
            promotion_epoch: AtomicU64::new(0),
            progress: Arc::new(ReplicaProgress::with_epoch(Arc::clone(&control_epoch))),
            self_port: AtomicU32::new(u32::from(self_port)),
            control_epoch,
        }
    }

    /// The shared gate-invalidation counter (see the field doc).
    /// `RuntimeState::build` stores a clone so catalog/scope writers
    /// can bump the same counter.
    pub(crate) fn control_epoch_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.control_epoch)
    }

    /// Step ② of the writer protocol: publish a gate-authority change.
    /// The preceding field write may stay `Relaxed` — an Acquire load
    /// of the bumped counter synchronizes-with this `Release`
    /// `fetch_add`, so the observer's rebuild sees the new field.
    fn bump_epoch(&self) {
        self.control_epoch.fetch_add(1, Ordering::Release);
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
    /// Production transitions reach the stop sequence through
    /// [`Self::start_runners`] (retarget) or
    /// [`Self::promote_stop_runners`] (promotion / shutdown); this
    /// bare wrapper exists for the state tests that exercise the
    /// teardown in isolation.
    #[cfg(test)]
    pub(crate) fn stop_runners(&self) {
        let _transition = self.retarget.lock().expect("retarget poisoned");
        self.stop_runners_inner();
    }

    /// Transition body — caller holds the `retarget` lock.
    fn stop_runners_inner(&self) {
        self.is_replica.store(false, Ordering::Relaxed);
        self.bump_epoch();
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
    pub(crate) fn start_runners(&self, upstream: (IpAddr, u16)) -> Result<(), CmdError> {
        if self.inboxes.lock().expect("inboxes poisoned").is_some() {
            return Err(CmdError::Wire("replica inboxes not wired into a runtime (kevy::serve required)"));
        }
        let _transition = self.retarget.lock().expect("retarget poisoned");
        // Stop any prior fleet before installing the new one. The old
        // runners' threads block on `next_event` reads; shutdown()
        // shuts down their sockets so the reads unblock and join
        // completes within ~one event.
        self.stop_runners_inner();
        let (host, port_base) = upstream;
        // Size the per-runner applied-offset registry BEFORE spawning —
        // a runner's first heartbeat may land before this function
        // returns, and its slot must already exist.
        let runner_count = if self.single_source { 1 } else { self.senders.len() };
        self.progress.size_runner_slots(runner_count);
        *self.runners.lock().expect("runners poisoned") = self.spawn_fleet(host, port_base);
        *self.upstream.lock().expect("upstream poisoned") = Some(upstream);
        // AFTER the internal stop_runners above (which clears the
        // flag) — the role flips to replica only once the new fleet
        // is installed.
        self.is_replica.store(true, Ordering::Relaxed);
        self.bump_epoch();
        Ok(())
    }

    /// Spawn the runner fleet for `(host, port_base)`. Id
    /// `kevy-replica-<client_port>#<stream>`: the port prefix keeps
    /// replicas in distinct ack slots + names one INFO entry per
    /// process. Single-source spawns one routing runner; the fleet
    /// model spawns one runner per shard at `port_base + shard_id`.
    fn spawn_fleet(&self, host: IpAddr, port_base: u16) -> Vec<ReplicaRunner> {
        let self_port = self.self_port.load(Ordering::Relaxed);
        if self.single_source {
            return vec![ReplicaRunner::spawn_routed(
                (host, port_base),
                format!("kevy-replica-{self_port}#s"),
                self.senders.clone(),
                0,
                Arc::clone(&self.progress),
            )];
        }
        let mut fleet = Vec::with_capacity(self.senders.len());
        for (shard_id, sender) in self.senders.iter().enumerate() {
            let port = port_base.saturating_add(u16::try_from(shard_id).unwrap_or(u16::MAX));
            fleet.push(ReplicaRunner::spawn(
                (host, port),
                format!("kevy-replica-{self_port}#{shard_id}"),
                sender.clone(),
                shard_id,
                Arc::clone(&self.progress),
            ));
        }
        fleet
    }

    /// Stop runners as part of a PROMOTION (`REPLICAOF NO ONE` on a
    /// following replica, or an election win). When this node really
    /// was a replica, the promotion counter bumps so every shard
    /// fences its offset space (feed generation bump — see
    /// `kevy_rt::LiveRuntimeConfig::promotion_epoch`). A plain
    /// [`Self::stop_runners`] (retarget teardown, shutdown) never bumps.
    pub(crate) fn promote_stop_runners(&self) {
        // Under the transition lock the was-replica check and the
        // epoch bump are one atomic step — two racing promotions
        // (elect callback + concurrent REPLICAOF NO ONE) can no
        // longer both observe `true` and double-bump.
        let _transition = self.retarget.lock().expect("retarget poisoned");
        let was_replica = self.is_replica();
        self.stop_runners_inner();
        if was_replica {
            self.promotion_epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn is_replica(&self) -> bool {
        self.is_replica.load(Ordering::Relaxed)
    }

    /// Restart-role clamp: mark this node a replica WITHOUT any
    /// runners — a restarted quorum member holds writes until an
    /// election outcome flips the flag (win → stop_runners clears it).
    pub(crate) fn force_replica_flag(&self) {
        self.is_replica.store(true, Ordering::Relaxed);
        self.bump_epoch();
    }

    pub(crate) fn set_read_only(&self, on: bool) {
        self.read_only.store(on, Ordering::Relaxed);
        self.bump_epoch();
    }

    pub(crate) fn read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed)
    }

    pub(crate) fn set_min_replicas(&self, n: u32) {
        self.min_replicas.store(n, Ordering::Relaxed);
        self.bump_epoch();
    }

    pub(crate) fn set_max_staleness_ms(&self, v: u64) {
        self.max_staleness_ms.store(v, Ordering::Relaxed);
        self.bump_epoch();
    }

    pub(crate) fn set_quiesce(&self, target: Option<String>) {
        *self.quiesce_to.lock().expect("quiesce_to poisoned") = target;
        self.bump_epoch();
    }

    pub(crate) fn quiesce_active(&self) -> bool {
        self.quiesce_to.lock().expect("quiesce_to poisoned").is_some()
    }

    /// Flip the quorum lease fence. Returns whether the
    /// flag CHANGED (callers log transitions only).
    pub(crate) fn set_quorum_fence(&self, on: bool) -> bool {
        let changed = self.quorum_fenced.swap(on, Ordering::Relaxed) != on;
        if changed {
            self.bump_epoch();
        }
        changed
    }

    /// Cold-path gate input: could a client write be denied
    /// right now? Mirrors [`Self::write_denied_reply`]'s deny set —
    /// `false` here means the slow path would certainly answer `None`.
    /// `min_replicas > 0` on a primary always reports `true`: the
    /// healthy-replica count is view-derived and changes without an
    /// epoch bump, so it must be re-judged per write.
    pub(crate) fn write_possibly_gated(&self) -> bool {
        self.quorum_fenced.load(Ordering::Relaxed)
            || self.quiesce_active()
            || (self.is_replica() && self.read_only())
            || (!self.is_replica() && self.min_replicas.load(Ordering::Relaxed) > 0)
    }

    /// Cold-path gate input: could a client read be denied?
    /// Staleness is a time condition — the bit only says "bounded
    /// replica"; the slow path loads the live heartbeat. A raised
    /// loading flag (full-resync snapshot ship in flight) gates reads
    /// regardless of the staleness knob; its flip bumps the control
    /// epoch, so the cached bit tracks it.
    pub(crate) fn read_possibly_gated(&self) -> bool {
        self.is_replica()
            && (self.max_staleness_ms.load(Ordering::Relaxed) > 0 || self.progress.loading())
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
    /// replica-side election offset. Comparable with the
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

    /// Whether a full-resync snapshot ship is in flight (the INFO
    /// `loading` gauge; reads answer `-LOADING` while true).
    pub(crate) fn loading(&self) -> bool {
        self.progress.loading()
    }

    /// Refuse reads on a replica mid-way through a full-resync
    /// snapshot load (`-LOADING`, strongest — the visible keyspace is
    /// about to be replaced wholesale) or one whose primary heartbeat
    /// is older than the staleness bound (`-STALE`). Primaries and
    /// un-bounded replicas outside a resync never refuse (the common
    /// case is two relaxed atomic loads).
    pub(crate) fn read_denied_reply(&self) -> Option<Vec<u8>> {
        if !self.is_replica() {
            return None;
        }
        if self.progress.loading() {
            return Some(b"-LOADING kevy is loading the dataset in memory\r\n".to_vec());
        }
        let bound = self.max_staleness_ms.load(Ordering::Relaxed);
        if bound == 0 {
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
    /// the write may proceed. Slow path only: callers reach
    /// here after the per-shard `WRITE_GATED` bit fired, so taking the
    /// quiesce mutex to render the error text is off the common path.
    /// `healthy_replicas` supplies the answering shard's view-derived
    /// replica count (the view lives in `ShardCtx`, which this
    /// runtime-wide state deliberately doesn't know about); it is only
    /// invoked on the primary min-replicas branch.
    pub(crate) fn write_denied_reply(
        &self,
        healthy_replicas: impl FnOnce() -> usize,
    ) -> Option<Vec<u8>> {
        if self.quorum_fenced.load(Ordering::Relaxed) {
            return Some(b"-NOREPLICAS primary lost quorum; writes fenced\r\n".to_vec());
        }
        if let Some(t) = self.quiesce_to.lock().expect("quiesce_to poisoned").as_ref() {
            return Some(format!("-QUIESCED migrating to {t}\r\n").into_bytes());
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
        if min > 0 && healthy_replicas() < min as usize {
            return Some(b"-NOREPLICAS Not enough good replicas to write.\r\n".to_vec());
        }
        None
    }
}

#[cfg(test)]
#[path = "replication_tests.rs"]
mod tests;
