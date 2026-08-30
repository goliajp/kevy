//! Per-shard private zone + the gate-bit cache.
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

// ───────────── gate bits ─────────────
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
/// The shared index catalog is nonempty — writes feed
/// `index_runtime::on_write`.
pub(crate) const IDX_NONEMPTY: u32 = 1 << 3;
/// The shared view catalog is nonempty — writes feed
/// `view_runtime::on_write`.
pub(crate) const VIEW_NONEMPTY: u32 = 1 << 4;
/// The shared TABLE catalog is nonempty — writes may take the packed
/// representation.
///
/// Separate from `IDX_NONEMPTY` on purpose: a table that declares columns
/// and no index is legal, and its rows are as declared as any other. Hanging
/// the representation off the index gate made a convenience of implementation
/// into a precondition of the feature, and the rows silently kept the general
/// form.
pub(crate) const TABLE_NONEMPTY: u32 = 1 << 5;

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
    if state.catalogs.index_nonempty() {
        bits |= IDX_NONEMPTY;
    }
    if state.catalogs.view_nonempty() {
        bits |= VIEW_NONEMPTY;
    }
    if state.catalogs.table_nonempty() {
        bits |= TABLE_NONEMPTY;
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
    /// This shard's AOF on-disk format (0 off / 1 v1 / 2 v2), fed per
    /// tick beside `persist_stats`.
    aof_format: Cell<u8>,
    /// One-shot boot-replay verdict (dropped bytes, corrupt) published via
    /// `Commands::on_replay_report`; static after boot.
    replay_report: Cell<(u64, bool)>,
    /// Per-tick replication view (`Commands::on_replication_view`).
    /// Stale by at most one tick interval (default 100 ms); all-default
    /// when this shard has no `ReplicationSource` installed.
    replication_view: RefCell<ReplicationView>,
    /// This shard's slice of every declared index (index-follows-key),
    /// refreshed lazily against the shared catalog generation.
    pub(crate) indexes: RefCell<crate::index_runtime::ShardIndexes>,
    /// This shard's packing backfill — the rows a table declaration
    /// found already there. Same lifecycle as `indexes`.
    pub(crate) packing: RefCell<crate::table_runtime::PackBackfill>,
    /// This shard's view states — same lifecycle as `indexes`.
    ///
    /// (The per-shard `LuaHost` is NOT here: it is `!Send` — luna-core's
    /// `Vm` holds `Rc`s — and `kevy_rt::Commands` requires `Send`, so
    /// it parks in `kevy_lua_host::with_thread_host`'s thread slot,
    /// the steel-side twin of this zone.)
    pub(crate) views: RefCell<crate::view_runtime::ShardViews>,
    /// Gate cache — see the bit constants above.
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
        self.ingesting_prefix.borrow().as_ref().is_some_and(|p| key.starts_with(p))
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

    pub(crate) fn set_replay_report(&self, dropped_bytes: u64, corrupt: bool) {
        self.replay_report.set((dropped_bytes, corrupt));
    }

    pub(crate) fn replay_report(&self) -> (u64, bool) {
        self.replay_report.get()
    }

    pub(crate) fn set_persist_stats(&self, in_flight: bool, aof_rewrites_total: u64) {
        self.persist_stats.set((in_flight, aof_rewrites_total));
    }

    /// One connection closed for crossing the query-buffer cap.
    ///
    /// Counted at the DECISION, not at the close completing — which is
    /// the distinction the counter exists to make visible.
    pub(crate) fn note_query_buffer_exceeded(&self) {
        self.with_stats_slot(|st| {
            st.query_buffer_disconnections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// Fold this tick's lateness into the shard's stall gauge
    /// (`fetch_max` — the gauge is a high-water mark, not a sum).
    pub(crate) fn note_tick_gap(&self, excess_us: u64) {
        self.with_stats_slot(|st| {
            st.tick_gap_max_us.fetch_max(excess_us, std::sync::atomic::Ordering::Relaxed);
            // The reactor calls this exactly once per tick BODY, so the
            // count rides along for free and turns the max gauge into a
            // pair a reader can tell apart: one late tick vs a slow one.
            st.ticks_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    }

    pub(crate) fn set_aof_format(&self, format: u8) {
        self.aof_format.set(format);
    }

    pub(crate) fn aof_format(&self) -> u8 {
        self.aof_format.get()
    }

    /// `(in_flight, aof_rewrites_total)` — see the field doc.
    pub(crate) fn persist_stats(&self) -> (bool, u64) {
        self.persist_stats.get()
    }

    pub(crate) fn set_replication_view(&self, view: ReplicationView) {
        *self.replication_view.borrow_mut() = view;
    }

    /// Replicas with a live connection AND at least one real ACK whose
    /// age is within `max_lag_ms`, per this shard's latest view tick
    /// (an acked offset of 0 counts: it is an empty replica's heartbeat
    /// ACK, and min-replicas would deadlock a fresh pair otherwise).
    /// Feeds the min-replicas write gate (`min_replicas_max_lag_ms`).
    pub(crate) fn healthy_replica_count(&self, max_lag_ms: u32) -> usize {
        self.replication_view
            .borrow()
            .replicas
            .iter()
            .filter(|(_, _, _, _, acked)| {
                acked.is_some_and(|a| a.ack_age_ms <= u64::from(max_lag_ms))
            })
            .count()
    }

    /// The gate read protocol: one `control_epoch` Acquire load; on an
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
        let ack = |off| Some(kevy_rt::ReplicaAck { acked_offset: off, ack_age_ms: 0 });
        shard.set_replication_view(ReplicationView {
            replicas: vec![
                ("r1".into(), ip, 1, 5, ack(5)),
                ("r2".into(), ip, 2, 5, None),
                ("r3".into(), ip, 3, 5, ack(0)),
            ],
        });
        assert_eq!(shard.healthy_replica_count(10_000), 2);
    }

    #[test]
    fn healthy_replica_count_excludes_acks_past_the_lag_window() {
        let shard = ShardCtx::default();
        let ip = std::net::Ipv4Addr::LOCALHOST;
        let ack = |age_ms| Some(kevy_rt::ReplicaAck { acked_offset: 5, ack_age_ms: age_ms });
        shard.set_replication_view(ReplicationView {
            // One fresh ACK, one exactly at the window edge (counts),
            // one past it (a stalled replica must not satisfy the gate).
            replicas: vec![
                ("r1".into(), ip, 1, 5, ack(0)),
                ("r2".into(), ip, 2, 5, ack(10_000)),
                ("r3".into(), ip, 3, 5, ack(10_001)),
            ],
        });
        assert_eq!(shard.healthy_replica_count(10_000), 2);
    }
}
