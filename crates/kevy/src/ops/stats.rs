//! Process-wide per-shard observability slots for `INFO` (Memory / Keyspace /
//! Stats sections).
//!
//! The server runs one independent [`Store`](kevy_store::Store) per shard, so
//! any single shard only sees its own slice of the keyspace. `INFO` is
//! answered on whichever shard the connection landed on — without aggregation
//! it would report ~1/Nth of `used_memory`, an empty Keyspace, etc. (the same
//! single-shard-view trap `DBSIZE` avoids by fanning out).
//!
//! Each shard owns one [`ShardStats`] slot. Gauges (`used_memory`, `keys`, …)
//! are **overwritten** with the shard's current absolute value on every
//! reactor tick; counters (`commands_processed`, `connections_received`) are
//! **added to** in the hot path. Summing every slot is correct for both. The
//! values are at most one tick (default 100 ms) stale — fine for INFO, which
//! is a snapshot by contract.
//!
//! Lock-free on the hot path: `on_shard_start` caches this shard's slot in a
//! thread-local `Arc`, so publish + counter bumps touch only relaxed atomics.
//! `INFO` (cold) takes the registry read lock once to sum the slots.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use kevy_store::Store;

/// One shard's observability slot. All atomics are `Relaxed`: these are
/// statistics, never used to establish happens-before.
#[derive(Default)]
pub(crate) struct ShardStats {
    pub used_memory: AtomicU64,
    pub used_memory_peak: AtomicU64,
    pub keys: AtomicU64,
    pub expires: AtomicU64,
    pub expired_keys: AtomicU64,
    pub evicted_keys: AtomicU64,
    pub commands_processed: AtomicU64,
    pub connections_received: AtomicU64,
}

/// Process-wide slot registry, grown lazily to fit each shard index the
/// first time it registers. Read once per `INFO`; written under the lock
/// only while growing (first tick of the highest-numbered shard).
static SLOTS: RwLock<Vec<Arc<ShardStats>>> = RwLock::new(Vec::new());

thread_local! {
    /// This reactor thread's slot (thread-per-core: thread == shard), cached
    /// by [`register_shard`] so the publish + hot-path counter bumps avoid the
    /// registry lock. `None` outside a reactor thread (tests, embedded).
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

/// Get-or-create shard `shard`'s slot in the registry.
fn slot(shard: usize) -> Arc<ShardStats> {
    {
        let r = SLOTS.read().expect("stats SLOTS poisoned");
        if let Some(s) = r.get(shard) {
            return s.clone();
        }
    }
    let mut w = SLOTS.write().expect("stats SLOTS poisoned");
    while w.len() <= shard {
        w.push(Arc::new(ShardStats::default()));
    }
    w[shard].clone()
}

/// Cache this thread's slot for lock-free publish/counter access. Called from
/// `KevyCommands::on_shard_start` (same place the cluster shard-id is stashed).
pub(crate) fn register_shard(shard: usize) {
    let s = slot(shard);
    LOCAL.with(|c| *c.borrow_mut() = Some(s));
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

/// Process-wide totals, summed across every registered shard slot.
#[derive(Default)]
pub(crate) struct Totals {
    pub used_memory: u64,
    pub used_memory_peak: u64,
    pub keys: u64,
    pub expires: u64,
    pub expired_keys: u64,
    pub evicted_keys: u64,
    pub commands_processed: u64,
    pub connections_received: u64,
}

/// Sum every shard's slot for the process-wide `INFO` view.
pub(crate) fn aggregate() -> Totals {
    let r = SLOTS.read().expect("stats SLOTS poisoned");
    let mut t = Totals::default();
    for s in r.iter() {
        t.used_memory += s.used_memory.load(Relaxed);
        t.used_memory_peak += s.used_memory_peak.load(Relaxed);
        t.keys += s.keys.load(Relaxed);
        t.expires += s.expires.load(Relaxed);
        t.expired_keys += s.expired_keys.load(Relaxed);
        t.evicted_keys += s.evicted_keys.load(Relaxed);
        t.commands_processed += s.commands_processed.load(Relaxed);
        t.connections_received += s.connections_received.load(Relaxed);
    }
    t
}

// ───────────── instantaneous_ops_per_sec ─────────────
//
// Redis samples (time, total_commands) into a ring on its serverCron and
// reports the average rate over the window. We do the same: the lead shard
// (0) pushes one sample per reactor tick; INFO divides the command delta by
// the time delta across the retained window (~1.6 s at the default 100 ms
// tick × 16 samples). On-demand two-INFO-call deltas would be meaningless,
// so the periodic sampler is the orthodox shape.

/// `(elapsed_ms_since_start, total_commands_processed)`.
static OPS_RING: Mutex<Vec<(u128, u64)>> = Mutex::new(Vec::new());
/// Process-start anchor for a monotonic millisecond clock.
static START: OnceLock<Instant> = OnceLock::new();
/// Retained samples — 16 × 100 ms default tick ≈ a 1.6 s window.
const OPS_WINDOW: usize = 16;

fn elapsed_ms() -> u128 {
    START.get_or_init(Instant::now).elapsed().as_millis()
}

/// Push one ops-per-sec sample — a no-op except on shard 0, so the ring
/// advances once per tick rather than once per shard per tick. Called from
/// `on_shard_tick`.
pub(crate) fn sample_ops_if_lead() {
    if LOCAL_SHARD.with(std::cell::Cell::get) != 0 {
        return;
    }
    let total = aggregate().commands_processed;
    let mut ring = OPS_RING.lock().expect("OPS_RING poisoned");
    ring.push((elapsed_ms(), total));
    if ring.len() > OPS_WINDOW {
        let drop = ring.len() - OPS_WINDOW;
        ring.drain(0..drop);
    }
}

/// Average commands/sec over the retained sample window. `current` is the
/// live process-wide command total (so the most recent traffic counts even
/// between samples). Returns 0 until two samples span a non-zero interval.
pub(crate) fn instantaneous_ops_per_sec(current: u64) -> u64 {
    let ring = OPS_RING.lock().expect("OPS_RING poisoned");
    let Some(&(oldest_ms, oldest_cmds)) = ring.first() else {
        return 0;
    };
    let dt_ms = elapsed_ms().saturating_sub(oldest_ms);
    if dt_ms == 0 {
        return 0;
    }
    let dc = current.saturating_sub(oldest_cmds);
    ((u128::from(dc) * 1000) / dt_ms) as u64
}
