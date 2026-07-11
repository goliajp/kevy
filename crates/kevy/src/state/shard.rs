//! Per-shard private zone (K-103 W4) + the gate-bit cache (W5).
//!
//! [`ShardCtx`] lives inside [`KevyCommands`](crate::KevyCommands);
//! the manual `Clone` impl gives every clone a **fresh** `ShardCtx`,
//! so nothing here ever leaks across shards. Interior mutability is
//! `Cell` / `RefCell` — a shard is single-threaded by construction
//! (thread-per-core), so no atomics are needed. This zone replaces
//! every `thread_local!` the kevy crate used to keep: since each
//! shard's `KevyCommands` clone is only touched by its own reactor
//! thread, a field here is exactly as private as a thread-local and
//! cheaper to reach (no TLS addressing).

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::RuntimeState;
use super::obs::ShardStats;
use crate::ops::replication::ReplicationView;

// ───────────── gate bits (K-103 W5) ─────────────
//
// One `control_epoch` Acquire load per use answers "did any cold-side
// authority change?"; on a hit the cached bits below stand in for the
// 4-7 shared loads the availability gates used to pay per command.
// Bits answer "possibly gated" only — a set bit sends the command to
// the precise (and slower) judge; a clear bit is a guaranteed pass.

/// A client write could be denied (fence / quiesce / read-only
/// replica / min-replicas configured).
pub(crate) const WRITE_GATED: u32 = 1 << 0;
/// A client read could be denied (bounded-staleness replica). The
/// staleness deadline itself is a time condition — never cached.
pub(crate) const READ_GATED: u32 = 1 << 1;
/// `[cluster] scopes` declared — writes must consult `route_write`.
pub(crate) const SCOPE_ACTIVE: u32 = 1 << 2;
/// The process-global index catalog is nonempty — writes feed
/// `index_runtime::on_write`.
pub(crate) const IDX_NONEMPTY: u32 = 1 << 3;
/// The process-global view catalog is nonempty — writes feed
/// `view_runtime::on_write`.
pub(crate) const VIEW_NONEMPTY: u32 = 1 << 4;

/// The per-shard gate cache: bits valid for exactly one value of
/// `RuntimeState`'s control epoch.
#[derive(Clone, Copy)]
pub(crate) struct GateCache {
    epoch: u64,
    bits: u32,
}

impl Default for GateCache {
    fn default() -> Self {
        // The counter starts at 0 and only increments, so `u64::MAX`
        // is unreachable — the first `gate_bits` call always rebuilds.
        Self { epoch: u64::MAX, bits: 0 }
    }
}

/// Cold-path recompute of every gate bit from the authority fields.
fn rebuild_gate(state: &RuntimeState) -> u32 {
    let mut bits = 0;
    if state.replication.write_possibly_gated() {
        bits |= WRITE_GATED;
    }
    if state.replication.read_possibly_gated() {
        bits |= READ_GATED;
    }
    if state.scope.is_active() {
        bits |= SCOPE_ACTIVE;
    }
    if crate::index_runtime::catalog_nonempty() {
        bits |= IDX_NONEMPTY;
    }
    if crate::view_runtime::catalog_nonempty() {
        bits |= VIEW_NONEMPTY;
    }
    bits
}

/// Per-shard private zone. See the module doc.
#[derive(Default)]
pub(crate) struct ShardCtx {
    /// This shard's id, written by `Commands::on_shard_start`. `None`
    /// outside a reactor thread (embedded / tests / the runtime main
    /// thread) — readers fall back to shard 0, the same convention the
    /// pre-4.0 thread-locals used.
    shard_id: Cell<Option<usize>>,
    /// Set for the duration of a `MOVE-SCOPE-INGEST` dispatch.
    /// `RuntimeState::route_write` checks this FIRST and treats a
    /// matching key as locally writable — the target node accepts the
    /// source's reconstruction commands without bouncing them back via
    /// `-MISDIRECTED`.
    ingesting_prefix: RefCell<Option<Vec<u8>>>,
    /// This shard's INFO-stats slot, cached by `on_shard_start` from
    /// `ObsState::slot(shard)` so gauge publishes touch only relaxed
    /// atomics. `None` outside a reactor thread, or when the runtime
    /// was built with more shards than the state was sized for.
    stats_slot: RefCell<Option<Arc<ShardStats>>>,
    /// Hot-path command / connection counters — plain `Cell`s bumped
    /// per event, published to the shared slot atomics on each tick
    /// by `ops::stats::publish_gauges`.
    cmds: Cell<u64>,
    conns: Cell<u64>,
    /// The answering shard's background-persistence view, refreshed by
    /// the reactor tick via `Commands::on_persist_stats`. Stale by at
    /// most one tick interval. `(in_flight, aof_rewrites_total)`.
    persist_stats: Cell<(bool, u64)>,
    /// Per-tick replication view (`Commands::on_replication_view`).
    /// Stale by at most one tick interval (default 100 ms); all-default
    /// when this shard has no `ReplicationSource` installed.
    replication_view: RefCell<ReplicationView>,
    /// This shard's slice of every declared index (index-follows-key),
    /// refreshed lazily against the global catalog generation.
    pub(crate) indexes: RefCell<crate::index_runtime::ShardIndexes>,
    /// This shard's view states — same lifecycle as `indexes`.
    ///
    /// (The per-shard `LuaHost` is NOT here: it is `!Send` — luna's
    /// `Vm` holds `Rc`s — and `kevy_rt::Commands` requires `Send`, so
    /// it parks in `kevy_lua_host::with_thread_host`'s thread slot,
    /// the steel-side twin of this zone.)
    pub(crate) views: RefCell<crate::view_runtime::ShardViews>,
    /// W5 gate cache — see the bit constants above.
    gate: Cell<GateCache>,
}

impl ShardCtx {
    pub(crate) fn set_shard_id(&self, shard: usize) {
        self.shard_id.set(Some(shard));
    }

    /// This shard's id, defaulting to 0 outside a reactor thread.
    pub(crate) fn shard_id(&self) -> usize {
        self.shard_id.get().unwrap_or(0)
    }

    /// Strictly shard 0 of a running reactor — non-reactor contexts
    /// (which *report* as shard 0) don't lead. Gates the process-wide
    /// ops-per-sec sampler to one pusher per tick.
    pub(crate) fn is_lead_shard(&self) -> bool {
        self.shard_id.get() == Some(0)
    }

    /// RAII guard for the MOVE-SCOPE-INGEST window. Cleared on drop;
    /// nested guards aren't expected (a recursion into another
    /// MOVE-SCOPE-INGEST inside one would be a bug — the inner ingest
    /// silently inherits the outer's prefix until both drop).
    pub(crate) fn ingest_guard(&self, prefix: Vec<u8>) -> IngestGuard<'_> {
        *self.ingesting_prefix.borrow_mut() = Some(prefix);
        IngestGuard { shard: self }
    }

    pub(crate) fn ingesting_matches(&self, key: &[u8]) -> bool {
        self.ingesting_prefix
            .borrow()
            .as_ref()
            .is_some_and(|p| key.starts_with(p))
    }

    pub(crate) fn set_stats_slot(&self, slot: Option<Arc<ShardStats>>) {
        *self.stats_slot.borrow_mut() = slot;
    }

    /// Run `f` against this shard's stats slot if one is registered.
    pub(crate) fn with_stats_slot(&self, f: impl FnOnce(&ShardStats)) {
        if let Some(s) = self.stats_slot.borrow().as_ref() {
            f(s);
        }
    }

    /// Count one processed client command (hot path — a single `Cell`
    /// increment).
    #[inline]
    pub(crate) fn add_command(&self) {
        self.cmds.set(self.cmds.get().wrapping_add(1));
    }

    /// Count one accepted connection.
    #[inline]
    pub(crate) fn add_connection(&self) {
        self.conns.set(self.conns.get().wrapping_add(1));
    }

    /// `(commands, connections)` counted so far on this shard.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (self.cmds.get(), self.conns.get())
    }

    pub(crate) fn set_persist_stats(&self, in_flight: bool, aof_rewrites_total: u64) {
        self.persist_stats.set((in_flight, aof_rewrites_total));
    }

    /// `(in_flight, aof_rewrites_total)` — see the field doc.
    pub(crate) fn persist_stats(&self) -> (bool, u64) {
        self.persist_stats.get()
    }

    pub(crate) fn set_replication_view(&self, view: ReplicationView) {
        *self.replication_view.borrow_mut() = view;
    }

    /// Snapshot the answering shard's replication view. Default
    /// (offset=0, no replicas) when replication is off on this shard.
    pub(crate) fn replication_view(&self) -> ReplicationView {
        self.replication_view.borrow().clone()
    }

    /// v3.14 D5 — replicas with a live connection AND at least one
    /// real ACK, per this shard's latest view tick (`Some(0)` counts:
    /// an empty replica's heartbeat ACK, or min-replicas deadlocks a
    /// fresh pair). Feeds the min-replicas write gate.
    pub(crate) fn healthy_replica_count(&self) -> usize {
        self.replication_view
            .borrow()
            .replicas
            .iter()
            .filter(|(_, _, _, acked)| acked.is_some())
            .count()
    }

    /// The W5 read protocol: one `control_epoch` Acquire load; on an
    /// epoch hit the cached bits answer, otherwise the cold rebuild
    /// runs. The epoch is read BEFORE the rebuild — a writer racing
    /// past between the two leaves at most one command on bits that
    /// are *newer* than their tag (converges on the next load), never
    /// on stale bits under a fresh tag.
    #[inline]
    pub(crate) fn gate_bits(&self, state: &RuntimeState) -> u32 {
        let epoch = state.control_epoch().load(Ordering::Acquire);
        let cached = self.gate.get();
        if cached.epoch == epoch {
            return cached.bits;
        }
        let bits = rebuild_gate(state);
        self.gate.set(GateCache { epoch, bits });
        bits
    }
}

/// See [`ShardCtx::ingest_guard`].
pub(crate) struct IngestGuard<'a> {
    shard: &'a ShardCtx,
}

impl Drop for IngestGuard<'_> {
    fn drop(&mut self) {
        *self.shard.ingesting_prefix.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_guard_sets_and_clears_prefix() {
        let shard = ShardCtx::default();
        {
            let _g = shard.ingest_guard(b"app:".to_vec());
            assert!(shard.ingesting_matches(b"app:k1"));
            assert!(!shard.ingesting_matches(b"other:k"));
        }
        assert!(!shard.ingesting_matches(b"app:k1"));
    }

    #[test]
    fn lead_shard_requires_an_assigned_id() {
        let shard = ShardCtx::default();
        assert_eq!(shard.shard_id(), 0, "unassigned reports 0");
        assert!(!shard.is_lead_shard(), "…but does not lead");
        shard.set_shard_id(0);
        assert!(shard.is_lead_shard());
        shard.set_shard_id(3);
        assert!(!shard.is_lead_shard());
        assert_eq!(shard.shard_id(), 3);
    }

    #[test]
    fn gate_bits_cache_hits_until_epoch_moves() {
        let c = crate::KevyCommands::new();
        let state = c.state();
        let shard = ShardCtx::default();
        assert_eq!(shard.gate_bits(state), 0, "fresh default state: all clear");
        // Cold write: flip an authority field via its two-step setter.
        state.replication.set_quiesce(Some("x:1".into()));
        let bits = shard.gate_bits(state);
        assert_ne!(bits & WRITE_GATED, 0, "epoch moved → rebuild sees quiesce");
        state.replication.set_quiesce(None);
        assert_eq!(shard.gate_bits(state) & WRITE_GATED, 0);
    }

    #[test]
    fn healthy_replica_count_requires_an_ack() {
        let shard = ShardCtx::default();
        let ip = std::net::Ipv4Addr::LOCALHOST;
        shard.set_replication_view(ReplicationView {
            master_repl_offset: 10,
            replicas: vec![(ip, 1, 5, Some(5)), (ip, 2, 5, None), (ip, 3, 5, Some(0))],
        });
        assert_eq!(shard.healthy_replica_count(), 2);
    }
}
