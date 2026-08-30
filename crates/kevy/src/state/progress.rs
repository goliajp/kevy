//! [`ReplicaProgress`] — the narrow heartbeat/offset slice replica
//! runner threads write into. Split out of the replication-state
//! module to keep both files under the 500-LOC house rule.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Replica-side heartbeat view, written by every runner on
/// each `+PING`, read by INFO replication / the election offset sum.
/// The gauge fields are independent atomics — torn reads across
/// fields are harmless (runners tick at 1 Hz × shards, INFO reads are
/// rare).
///
/// This is the ONLY slice of state a replica runner thread captures
/// (`Arc<ReplicaProgress>`), keeping runner lifetimes decoupled from
/// [`ReplicationState`] and the state graph acyclic.
#[derive(Default)]
pub(crate) struct ReplicaProgress {
    /// Full-resync window count: raised per runner at `SnapshotBegin`,
    /// lowered when the shard-side APPLY of `SnapshotEnd` lands (the
    /// runner hands a drop-token to the apply event; runners also
    /// lower on link drop, so a mid-ship disconnect never strands the
    /// replica refusing reads). While non-zero, client reads answer
    /// `-LOADING` — the dataset is about to be replaced wholesale, so
    /// serving the pre-resync keyspace would serve a timeline the
    /// primary has already diverged from.
    loading: AtomicUsize,
    /// The instance's gate-invalidation counter, shared in by
    /// [`super::ReplicationState`] at construction (runner threads
    /// capture only this progress slice, yet a loading flip must
    /// invalidate every shard's cached gate bits).
    control_epoch: Arc<AtomicU64>,
    /// Primary-position gauge — `fetch_max` ACROSS runners: fine for
    /// INFO's "representative lag", wrong for election ordering
    /// (that's what [`Self::runner_offsets`] is for).
    primary_offset: AtomicU64,
    /// Applied-position gauge, `fetch_max` across runners like
    /// [`Self::primary_offset`].
    applied_offset: AtomicU64,
    last_ping_ms: AtomicU64,
    /// Per-runner applied-offset registry, `runner_slot` →
    /// this runner's replication-stream position. Sized by
    /// `start_runners` (one slot per runner: `nshards` in the fleet
    /// model, 1 in single-source), cleared by `stop_runners`.
    ///
    /// Why a registry next to the max-gauge: election candidate
    /// ordering needs the SUM of stream positions (same shape as the
    /// primary side's per-shard `master_repl_offset` sum). Max would
    /// let a node with ONE advanced shard tie a node where every
    /// shard is caught up.
    runner_offsets: Mutex<Box<[AtomicU64]>>,
    /// Per-runner last-seen UPSTREAM feed generation,
    /// learned from the gen-carrying heartbeat (`+PING <gen> <next>`).
    /// Sized / cleared with [`Self::runner_offsets`]. `0` = no
    /// heartbeat seen yet (real generations start at 1) — REPL.WAIT
    /// treats that as a gen mismatch, the conservative answer.
    upstream_gens: Mutex<Box<[u64]>>,
}

impl ReplicaProgress {
    /// Build a progress slice wired to the instance's shared
    /// gate-invalidation counter.
    pub(super) fn with_epoch(control_epoch: Arc<AtomicU64>) -> Self {
        Self { control_epoch, ..Self::default() }
    }

    /// Runner-side: one runner entered its snapshot-ship window. The
    /// 0 → 1 edge publishes a gate invalidation so every shard
    /// rebuilds its cached gate bits and starts refusing reads.
    pub(crate) fn begin_loading(&self) {
        if self.loading.fetch_add(1, Ordering::AcqRel) == 0 {
            self.control_epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// One runner left its snapshot-ship window (`SnapshotEnd`
    /// APPLIED by the last shard, or the link dropped mid-ship).
    /// The 1 → 0 edge re-opens reads.
    pub(crate) fn end_loading(&self) {
        if self.loading.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.control_epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// Whether any runner's full-resync snapshot ship is in flight.
    pub(crate) fn loading(&self) -> bool {
        self.loading.load(Ordering::Acquire) > 0
    }

    /// Runner-side: record one heartbeat observation. Gauges take the
    /// MAX per field across runners; `runner_slot` additionally files
    /// `applied` into this runner's own registry slot — the exact
    /// (non-max) per-stream position the election-offset sum is built
    /// from. `generation` is the upstream's feed generation off the
    /// gen-carrying heartbeat, filed per runner slot for REPL.WAIT gen
    /// matching.
    pub(crate) fn record_ping(
        &self,
        runner_slot: usize,
        generation: u64,
        primary_offset: u64,
        applied: u64,
    ) {
        self.primary_offset.fetch_max(primary_offset, Ordering::Relaxed);
        self.applied_offset.fetch_max(applied, Ordering::Relaxed);
        self.last_ping_ms.store(epoch_ms(), Ordering::Relaxed);
        self.record_applied(runner_slot, applied);
        let mut gens = self.upstream_gens.lock().expect("upstream_gens poisoned");
        if let Some(slot) = gens.get_mut(runner_slot) {
            *slot = generation;
        }
    }

    /// Runner-side: refresh this runner's applied slot without a full
    /// heartbeat observation — called on the 100 ms periodic ACK in
    /// the frame-apply path, so the election offset stays fresh under
    /// write load (pings alone only tick at 1 Hz). Plain store, not
    /// max: a resync-from-0 after a diverged rejoin genuinely rewinds
    /// the stream position and the election metric must tell the
    /// truth. An out-of-range slot is ignored (registry resize race).
    pub(crate) fn record_applied(&self, runner_slot: usize, applied: u64) {
        let slots = self.runner_offsets.lock().expect("runner_offsets poisoned");
        if let Some(slot) = slots.get(runner_slot) {
            slot.store(applied, Ordering::Relaxed);
        }
    }

    pub(super) fn size_runner_slots(&self, n: usize) {
        *self.runner_offsets.lock().expect("runner_offsets poisoned") =
            (0..n).map(|_| AtomicU64::new(0)).collect();
        *self.upstream_gens.lock().expect("upstream_gens poisoned") = vec![0; n].into_boxed_slice();
    }

    pub(super) fn clear_runner_slots(&self) {
        self.size_runner_slots(0);
    }

    pub(super) fn runner_offsets(&self) -> Vec<u64> {
        self.runner_offsets
            .lock()
            .expect("runner_offsets poisoned")
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect()
    }

    pub(super) fn upstream_gens(&self) -> Vec<u64> {
        self.upstream_gens.lock().expect("upstream_gens poisoned").to_vec()
    }

    pub(super) fn applied_offset_sum(&self) -> u64 {
        self.runner_offsets
            .lock()
            .expect("runner_offsets poisoned")
            .iter()
            .fold(0u64, |acc, v| acc.saturating_add(v.load(Ordering::Relaxed)))
    }

    pub(super) fn last_ping_ms(&self) -> u64 {
        self.last_ping_ms.load(Ordering::Relaxed)
    }

    pub(super) fn link_view(&self) -> (bool, u64, u64, u64) {
        let last = self.last_ping_ms();
        let age_ms = epoch_ms().saturating_sub(last);
        let up = last != 0 && age_ms < 3_000;
        let primary = self.primary_offset.load(Ordering::Relaxed);
        let applied = self.applied_offset.load(Ordering::Relaxed);
        (up, applied, primary.saturating_sub(applied), age_ms / 1000)
    }
}

pub(super) fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
