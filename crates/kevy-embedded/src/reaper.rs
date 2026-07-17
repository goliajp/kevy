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

#[cfg(feature = "persist")]

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
) -> io::Result<(Option<Arc<AtomicBool>>, Option<JoinHandle<()>>)> {
    match config.ttl_reaper {
        TtlReaperMode::Manual => Ok((None, None)),
        TtlReaperMode::Background => {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = stop.clone();
            let shards_t = shards.clone();
            let interval = config.reaper_interval;
            let samples = config.reaper_samples;
            let rounds = config.reaper_max_rounds;
            #[cfg(feature = "persist")]
            let policy = kevy_persist::RewritePolicy {
                pct: config.auto_aof_rewrite_pct,
                min_size: config.auto_aof_rewrite_min_size,
                bytes: config.auto_aof_rewrite_bytes,
                interval_secs: config.auto_aof_rewrite_interval_secs,
            };
            #[cfg(feature = "persist")]
            let sink = config.metric_sink.clone();
            let handle = std::thread::Builder::new()
                .name(String::from("kevy-embedded-reaper"))
                .spawn(move || {
                    #[cfg(feature = "persist")]
                    reaper_loop(shards_t, stop_t, interval, samples, rounds, policy, sink);
                    #[cfg(not(feature = "persist"))]
                    reaper_loop(shards_t, stop_t, interval, samples, rounds);
                })?;
            Ok((Some(stop), Some(handle)))
        }
    }
}

#[allow(clippy::too_many_arguments)] // reaper config knobs, all primitives
fn reaper_loop(
    shards: Shards,
    stop: Arc<AtomicBool>,
    interval: Duration,
    samples: usize,
    rounds: u32,
    #[cfg(feature = "persist")] policy: kevy_persist::RewritePolicy,
    #[cfg(feature = "persist")] sink: Option<MetricSink>,
) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        for shard in shards.iter() {
            {
                let mut g = lock_inner(shard);
                let _ = g.store.tick_expire(samples, rounds);
                let _ = g.store.tick_hash_ttl(64);
                // EverySec AOF fsync window check — runs from the same tick.
                #[cfg(feature = "persist")]
                if let Some(aof) = &mut g.aof {
                    let _ = aof.maybe_sync();
                }
            }
            // Non-blocking: holds the lock only for begin/finish, not the spill.
            #[cfg(feature = "persist")]
            concurrent_auto_rewrite(shard, policy, sink.as_ref());
        }
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
