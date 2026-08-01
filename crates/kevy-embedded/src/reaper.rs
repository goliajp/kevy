//! Background TTL reaper + non-blocking AOF auto-rewrite. Split out of
//! `store.rs` to keep it under the 500-LOC house cap; operates on the shared
//! [`Inner`] state via the same mutex the public `Store` methods use.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(feature = "persist")]
use std::time::Instant;

use crate::config::{Config, TtlReaperMode};
#[cfg(feature = "persist")]
use crate::metric::{KevyMetric, MetricSink};
use crate::store::{Inner, Shards};

/// Start the background TTL reaper thread, returning its stop signal +
/// join handle. `TtlReaperMode::Manual` returns `(None, None)` so the
/// caller-driven reap is in charge instead.
#[allow(clippy::type_complexity)] // inline tuple keeps the pair colocated
pub(crate) fn spawn_reaper(
    config: &Config,
    shards: &Shards,
    #[cfg(feature = "index")] tables: &Arc<crate::ops_table::TableReg>,
) -> io::Result<(Option<Arc<AtomicBool>>, Option<JoinHandle<()>>)> {
    match config.ttl_reaper {
        TtlReaperMode::Manual => Ok((None, None)),
        TtlReaperMode::Background => {
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn_loop(
                config,
                shards,
                stop.clone(),
                #[cfg(feature = "index")]
                tables,
            )?;
            Ok((Some(stop), Some(handle)))
        }
    }
}

/// The background half of [`spawn_reaper`]: capture the loop's config
/// and start the thread.
fn spawn_loop(
    config: &Config,
    shards: &Shards,
    stop_t: Arc<AtomicBool>,
    #[cfg(feature = "index")] tables: &Arc<crate::ops_table::TableReg>,
) -> io::Result<JoinHandle<()>> {
    let shards_t = shards.clone();
    #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
    let win = config.data_dir.clone().map(|d| (tables.clone(), d));
    #[cfg(not(all(feature = "index", feature = "persist", not(target_arch = "wasm32"))))]
    #[cfg(feature = "index")]
    let _ = tables;
    let (interval, samples, rounds) =
        (config.reaper_interval, config.reaper_samples, config.reaper_max_rounds);
    #[cfg(feature = "persist")]
    let (policy, sink) = rewrite_slot(config);
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    let tier: TierSpecOpt = config.tier_budget;
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    let tier: TierSpecOpt = ();
    std::thread::Builder::new()
        .name(String::from("kevy-embedded-reaper"))
        .spawn(move || {
            #[cfg(feature = "persist")]
            reaper_loop(
                shards_t,
                stop_t,
                interval,
                samples,
                rounds,
                tier,
                #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
                win,
                policy,
                sink,
            );
            #[cfg(not(feature = "persist"))]
            reaper_loop(
                shards_t,
                stop_t,
                interval,
                samples,
                rounds,
                tier,
                #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
                win,
            );
        })
}

/// The auto-rewrite policy + metric sink, captured from config.
#[cfg(feature = "persist")]
fn rewrite_slot(config: &Config) -> (kevy_persist::RewritePolicy, Option<MetricSink>) {
    (
        kevy_persist::RewritePolicy {
            pct: config.auto_aof_rewrite_pct,
            min_size: config.auto_aof_rewrite_min_size,
            bytes: config.auto_aof_rewrite_bytes,
            interval_secs: config.auto_aof_rewrite_interval_secs,
        },
        config.metric_sink.clone(),
    )
}

/// The reaper's tiering-config slot: the budget spec when the tier
/// backend is compiled in, `()` otherwise (keeps `reaper_loop`'s
/// signature cfg-stable at the call sites).
#[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
type TierSpecOpt = Option<crate::config::TierBudgetSpec>;
#[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
type TierSpecOpt = ();

#[allow(clippy::too_many_arguments)] // reaper config knobs, all primitives
fn reaper_loop(
    shards: Shards,
    stop: Arc<AtomicBool>,
    interval: Duration,
    samples: usize,
    rounds: u32,
    tier: TierSpecOpt,
    #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))] win: Option<(
        Arc<crate::ops_table::TableReg>,
        std::path::PathBuf,
    )>,
    #[cfg(feature = "persist")] policy: kevy_persist::RewritePolicy,
    #[cfg(feature = "persist")] sink: Option<MetricSink>,
) {
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    let _ = tier;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        for (shard_i, shard) in shards.iter().enumerate() {
            #[cfg(not(all(feature = "index", feature = "persist", not(target_arch = "wasm32"))))]
            let _ = shard_i;
            shard_upkeep(
                shard,
                samples,
                rounds,
                tier,
                shards.len(),
                #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
                win.as_ref().map(|(t, d)| (t, kevy_persist::layout::segs_dir(d, shard_i))),
            );
            // Non-blocking: holds the lock only for begin/finish, not the spill.
            #[cfg(feature = "persist")]
            concurrent_auto_rewrite(shard, policy, sink.as_ref());
        }
    }
}

/// One shard's locked tick body: TTL sweeps, the window tick, tiering
/// upkeep, and the everysec AOF window check.
fn shard_upkeep(
    shard: &Arc<RwLock<Inner>>,
    samples: usize,
    rounds: u32,
    tier: TierSpecOpt,
    nshards: usize,
    #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))] win: Option<(
        &Arc<crate::ops_table::TableReg>,
        std::path::PathBuf,
    )>,
) {
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    let _ = (tier, nshards);
    let mut g = lock_inner(shard);
    let _ = g.store.tick_expire(samples, rounds);
    let _ = g.store.tick_hash_ttl(64);
    // The window tick: reconcile per-index window runtimes against the
    // table catalog and slide any whose boundary moved. Persistent
    // stores only — memory-only deployments stay all-hot.
    #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
    if let Some((tables, segs_dir)) = win {
        let inner = &mut *g;
        crate::ops_index_window::window_tick(
            &mut inner.idx_segs,
            &mut inner.store,
            tables,
            &segs_dir,
        );
    }
    // Tiering upkeep: budget re-resolution (auto/percent re-probe the
    // memory bound) + the index/view floor feed, THEN continue any
    // spill the budgeted write-path batches left unfinished (no-op
    // when off/under budget).
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    crate::shard::tier_tick_upkeep(&mut g, tier, nshards);
    let _ = g.store.demote_step();
    let _ = g.store.tier_compact_tick();
    // EverySec AOF fsync window check — runs from the same tick.
    #[cfg(feature = "persist")]
    if let Some(aof) = &mut g.aof {
        let _ = aof.maybe_sync();
    }
}

/// **Non-blocking** auto-`BGREWRITEAOF`. Three phases bracket the lock so the
/// slow disk write happens with the lock *released* — application writes keep
/// flowing during the rewrite (feedback #2 "维护黑洞").
///
/// Phase 1 (locked): decide + `begin_concurrent_rewrite` — serialize the
/// keyspace to memory and start teeing live appends into a diff buffer.
/// Phase 2 (unlocked): spill the snapshot image to a temp file + fsync — the
/// expensive part, off the hot path.
/// Phase 3 (locked): `finish_concurrent_rewrite` — append the tee'd diff,
/// fsync, atomically swap over the live AOF, reopen.
///
/// On any failure the in-flight rewrite is aborted (live AOF untouched, no
/// data at risk) and the temp file removed.
#[cfg(feature = "persist")]
pub(crate) fn concurrent_auto_rewrite(
    inner: &Arc<RwLock<Inner>>,
    policy: kevy_persist::RewritePolicy,
    sink: Option<&MetricSink>,
) {
    let Some((start, view, tmp, before_bytes)) = begin_rewrite(inner, policy) else {
        return;
    };
    // Phase 2 — serialize the frozen view + fsync, lock released.
    let keys = match kevy_persist::dump_aof(&tmp, &view) {
        Ok((keys, _)) => keys,
        Err(e) => {
            eprintln!("kevy: embedded auto AOF rewrite (dump) failed: {e}");
            let mut g = lock_inner(inner);
            if let Some(aof) = &mut g.aof {
                aof.abort_concurrent_rewrite();
            }
            let _ = std::fs::remove_file(&tmp);
            return;
        }
    };
    // Phase 3 — append the diff, swap, reopen, under the lock.
    let mut g = lock_inner(inner);
    let Some(aof) = &mut g.aof else { return };
    match aof.finish_concurrent_rewrite(&tmp, keys) {
        Ok(stats) => {
            if let Some(sink) = sink {
                sink.emit(KevyMetric::Rewrite {
                    keys: stats.keys,
                    before_bytes,
                    after_bytes: stats.bytes,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
        }
        Err(e) => {
            eprintln!("kevy: embedded auto AOF rewrite (finish) failed: {e}");
            aof.abort_concurrent_rewrite();
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Phase 1 — decide + freeze the COW view + start the tee, under the lock.
/// O(n)-shallow (refcount bumps + key copies). `start` reads the clock only
/// past the no-op early-out — never on the common idle tick (and
/// `wasm32-unknown-unknown` has no `Instant`, so reading it up front traps).
/// `None` = below threshold or begin failed (already logged).
#[cfg(feature = "persist")]
#[allow(clippy::type_complexity)] // inline tuple keeps the phase-1 outputs colocated
fn begin_rewrite(
    inner: &Arc<RwLock<Inner>>,
    policy: kevy_persist::RewritePolicy,
) -> Option<(Instant, kevy_store::SnapshotView, std::path::PathBuf, u64)> {
    let mut g = lock_inner(inner);
    let ready = g.aof.as_ref().is_some_and(|a| a.rewrite_due(policy));
    if !ready {
        return None;
    }
    let start = Instant::now();
    let Inner { store, aof, .. } = &mut *g;
    let aof = aof.as_mut().expect("checked above");
    let before = aof.size_bytes();
    let view = store.collect_snapshot();
    match aof.begin_view_rewrite() {
        Ok(tmp) => Some((start, view, tmp, before)),
        Err(e) => {
            eprintln!("kevy: embedded auto AOF rewrite (begin) failed: {e}");
            None
        }
    }
}

/// Write-lock the inner state, recovering from a poisoned lock (a method panic
/// elsewhere left data intact in memory). The reaper mutates (reap + clock
/// refresh + rewrite), so it always takes the write side.
pub(crate) fn lock_inner(inner: &Arc<RwLock<Inner>>) -> RwLockWriteGuard<'_, Inner> {
    inner.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}
