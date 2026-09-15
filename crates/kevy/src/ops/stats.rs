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
        // Tiering gauges: one cheap struct read when on,
        // one branch + a zero-store of `enabled` when off.
        s.tier.enabled.store(u64::from(store.tier_enabled()), Relaxed);
        if store.tier_enabled() {
            let ts = store.tier_stats();
            s.tier.budget.store(ts.budget, Relaxed);
            s.tier.effective_target.store(ts.effective_target, Relaxed);
            s.tier.reserved_bytes.store(ts.reserved_bytes, Relaxed);
            s.tier.stub_bytes.store(ts.stub_bytes, Relaxed);
            s.tier.cold_keys.store(ts.cold_keys, Relaxed);
            s.tier.cold_bytes.store(ts.cold_bytes, Relaxed);
            s.tier.demotions_total.store(ts.demotions_total, Relaxed);
            s.tier.promotions_total.store(ts.promotions_total, Relaxed);
            s.tier.peek_preads_total.store(ts.peek_preads_total, Relaxed);
            s.tier.batch_submissions_total.store(ts.batch_submissions_total, Relaxed);
            s.tier.vlog_files.store(ts.vlog_files, Relaxed);
            s.tier.vlog_bytes.store(ts.vlog_bytes, Relaxed);
            s.tier.vlog_live_bytes.store(ts.vlog_live_bytes, Relaxed);
            s.tier.vlog_epoch.store(ts.vlog_epoch, Relaxed);
        }
        publish_alloc_gauges(s);
    });
}

/// Publish this shard's allocator terms. `thread_stats` answers for the
/// CALLING thread, and each shard owns its own heap — so this has to
/// happen here, on the shard thread, and cannot be done by whichever
/// shard INFO lands on.
///
/// The gate is `mapped > 0`, not `Some`. Compiling the feature in links
/// kevy-alloc; it does not make it the allocator — the
/// `#[global_allocator]` attribute does, and that lives in the binary,
/// so the library and everything that links it (tests, the embedded
/// API, an FFI host) can have the feature on and route every allocation
/// somewhere else. `thread_stats` still answers `Some` there: a heap is
/// created on demand and reports nine honest zeroes. Reading that as
/// "kevy-alloc, holding nothing" would have INFO name an allocator that
/// is not running. A heap that has never mapped a byte has served
/// nothing, so suppressing it costs no information.
#[cfg(feature = "kevy-alloc")]
fn publish_alloc_gauges(s: &crate::state::ShardStats) {
    let Some(a) = kevy_alloc::thread_stats().filter(|a| a.mapped > 0) else { return };
    s.alloc.mapped.store(a.mapped, Relaxed);
    s.alloc.live.store(a.live, Relaxed);
    s.alloc.rounding.store(a.rounding, Relaxed);
    s.alloc.cache.store(a.cache, Relaxed);
    s.alloc.span_free.store(a.span_free, Relaxed);
    s.alloc.returned.store(a.returned, Relaxed);
    s.alloc.virgin.store(a.virgin, Relaxed);
    s.alloc.hysteresis.store(a.hysteresis, Relaxed);
    s.alloc.segment_overhead.store(a.segment_overhead, Relaxed);
    s.alloc.large_count.store(a.large_count, Relaxed);
    s.alloc.spans_assigned.store(a.spans_assigned, Relaxed);
    // Last, so a reader that sees `reporting` sees the terms behind it.
    s.alloc.reporting.store(1, Relaxed);
}

#[cfg(not(feature = "kevy-alloc"))]
fn publish_alloc_gauges(_s: &crate::state::ShardStats) {}

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
