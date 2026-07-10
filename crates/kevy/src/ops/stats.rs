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
//! [`ShardStats`] per shard, preallocated). Gauges (`used_memory`,
//! `keys`, …) are **overwritten** with the shard's current absolute
//! value on every reactor tick; counters (`commands_processed`,
//! `connections_received`) are **added to** in the hot path. Summing
//! every slot is correct for both. The values are at most one tick
//! (default 100 ms) stale — fine for INFO, which is a snapshot by
//! contract.
//!
//! Lock-free on the hot path: `on_shard_start` caches this shard's slot in a
//! thread-local `Arc`, so publish + counter bumps touch only relaxed atomics.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use kevy_store::Store;

use crate::state::{ObsState, ShardStats};

thread_local! {
    /// This reactor thread's slot (thread-per-core: thread == shard), cached
    /// by [`register_shard`] so the publish + hot-path counter bumps avoid
    /// touching the shared state. `None` outside a reactor thread (tests,
    /// embedded).
    static LOCAL: std::cell::RefCell<Option<Arc<ShardStats>>> =
        const { std::cell::RefCell::new(None) };
    /// This thread's shard id, so exactly one shard (0) drives the
    /// process-wide ops-per-sec sampler. `usize::MAX` = not a reactor thread.
    static LOCAL_SHARD: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    /// Hot-path command / connection counters. Plain `Cell`s (no atomic, no
    /// contention — each lives on its own shard thread); published to the
    /// shared slot atomics on each tick by [`publish_gauges`]. Keeps the
    /// per-command cost to a single thread-local increment.
    static LOCAL_CMDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static LOCAL_CONNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Cache this thread's slot for lock-free publish/counter access. Called from
/// `KevyCommands::on_shard_start` (same place the cluster shard-id is stashed)
/// with the slot from `ObsState::slot(shard)`; `None` (a runtime with more
/// shards than the state was sized for) leaves this thread unregistered.
pub(crate) fn register_shard(shard: usize, slot: Option<Arc<ShardStats>>) {
    LOCAL.with(|c| *c.borrow_mut() = slot);
    LOCAL_SHARD.with(|c| c.set(shard));
}

/// Run `f` against this thread's slot if one is registered (no-op otherwise).
fn with_local(f: impl FnOnce(&ShardStats)) {
    LOCAL.with(|c| {
        if let Some(s) = c.borrow().as_ref() {
            f(s);
        }
    });
}

/// Publish this shard's current gauge + counter values to its slot. Called
/// once per reactor tick with the shard's live [`Store`]. Gauges come from the
/// store; the command / connection counters come from this thread's hot-path
/// `Cell`s.
pub(crate) fn publish_gauges(store: &Store) {
    let cmds = LOCAL_CMDS.with(std::cell::Cell::get);
    let conns = LOCAL_CONNS.with(std::cell::Cell::get);
    with_local(|s| {
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

/// Count one processed client command (hot path — a single thread-local
/// increment). Called from the reactor's per-command entry.
#[inline]
pub(crate) fn add_command() {
    LOCAL_CMDS.with(|c| c.set(c.get().wrapping_add(1)));
}

/// Count one accepted connection. Called from the reactor's accept path.
#[inline]
pub(crate) fn add_connection() {
    LOCAL_CONNS.with(|c| c.set(c.get().wrapping_add(1)));
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
pub(crate) fn sample_ops_if_lead(obs: &ObsState) {
    if LOCAL_SHARD.with(std::cell::Cell::get) != 0 {
        return;
    }
    obs.push_ops_sample(obs.aggregate().commands_processed);
}
