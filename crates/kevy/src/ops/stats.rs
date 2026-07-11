//! Per-shard observability plumbing for `INFO` (Memory / Keyspace /
//! Stats sections).
//!
//! The server runs one independent [`Store`](kevy_store::Store) per shard, so
//! any single shard only sees its own slice of the keyspace. `INFO` is
//! answered on whichever shard the connection landed on — without aggregation
//! it would report ~1/Nth of `used_memory`, an empty Keyspace, etc. (the same
//! single-shard-view trap `DBSIZE` avoids by fanning out).
//!
//! The slots themselves live in [`crate::state::ObsState`] (one
//! [`ShardStats`](crate::state::ShardStats) per shard, preallocated).
//! Gauges (`used_memory`, `keys`, …) are **overwritten** with the
//! shard's current absolute value on every reactor tick; counters
//! (`commands_processed`, `connections_received`) accumulate in the
//! shard zone's plain `Cell`s (`ShardCtx`) on the hot path and are
//! published here per tick. Summing every slot is correct for both.
//! The values are at most one tick (default 100 ms) stale — fine for
//! INFO, which is a snapshot by contract.

use std::sync::atomic::Ordering::Relaxed;

use kevy_store::Store;

use crate::state::{ObsState, ShardCtx};

/// Publish this shard's current gauge + counter values to its slot.
/// Called once per reactor tick (and on-demand by `INFO`, so the
/// answering shard is never stale) with the shard's live [`Store`].
/// Gauges come from the store; the command / connection counters come
/// from the shard zone's hot-path `Cell`s. No-op when this context has
/// no registered slot (embedded / tests).
pub(crate) fn publish_gauges(shard: &ShardCtx, store: &Store) {
    let (cmds, conns) = shard.counters();
    shard.with_stats_slot(|s| {
        s.used_memory.store(store.used_memory(), Relaxed);
        s.used_memory_peak.store(store.used_memory_peak(), Relaxed);
        s.keys.store(store.dbsize() as u64, Relaxed);
        s.expires.store(store.expires_count() as u64, Relaxed);
        s.expired_keys.store(store.expired_keys_total(), Relaxed);
        s.evicted_keys.store(store.evictions_total(), Relaxed);
        s.commands_processed.store(cmds, Relaxed);
        s.connections_received.store(conns, Relaxed);
    });
}

// ───────────── instantaneous_ops_per_sec ─────────────
//
// Redis samples (time, total_commands) into a ring on its serverCron and
// reports the average rate over the window. We do the same: the lead shard
// (0) pushes one sample per reactor tick; INFO divides the command delta by
// the time delta across the retained window (~1.6 s at the default 100 ms
// tick × 16 samples). On-demand two-INFO-call deltas would be meaningless,
// so the periodic sampler is the orthodox shape. The ring lives in
// [`ObsState`]; this helper adds the lead-shard gate.

/// Push one ops-per-sec sample — a no-op except on shard 0, so the ring
/// advances once per tick rather than once per shard per tick. Called from
/// `on_shard_tick`.
pub(crate) fn sample_ops_if_lead(shard: &ShardCtx, obs: &ObsState) {
    if !shard.is_lead_shard() {
        return;
    }
    obs.push_ops_sample(obs.aggregate().commands_processed);
}
