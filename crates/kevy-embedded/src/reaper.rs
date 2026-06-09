//! Background TTL reaper + non-blocking AOF auto-rewrite. Split out of
//! `store.rs` to keep it under the 500-LOC house cap; operates on the shared
//! [`Inner`] state via the same mutex the public `Store` methods use.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kevy_persist::Aof;

use crate::config::{Config, TtlReaperMode};
use crate::metric::{KevyMetric, MetricSink};
use crate::store::Inner;

/// Start the background TTL reaper thread, returning its stop signal +
/// join handle. `TtlReaperMode::Manual` returns `(None, None)` so the
/// caller-driven reap is in charge instead.
#[allow(clippy::type_complexity)] // inline tuple keeps the pair colocated
pub(crate) fn spawn_reaper(
    config: &Config,
    inner: &Arc<RwLock<Inner>>,
) -> io::Result<(Option<Arc<AtomicBool>>, Option<JoinHandle<()>>)> {
    match config.ttl_reaper {
        TtlReaperMode::Manual => Ok((None, None)),
        TtlReaperMode::Background => {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = stop.clone();
            let inner_t = inner.clone();
            let interval = config.reaper_interval;
            let samples = config.reaper_samples;
            let rounds = config.reaper_max_rounds;
            let rw_pct = config.auto_aof_rewrite_pct;
            let rw_min = config.auto_aof_rewrite_min_size;
            let sink = config.metric_sink.clone();
            let handle = std::thread::Builder::new()
                .name(String::from("kevy-embedded-reaper"))
                .spawn(move || {
                    reaper_loop(inner_t, stop_t, interval, samples, rounds, rw_pct, rw_min, sink)
                })?;
            Ok((Some(stop), Some(handle)))
        }
    }
}

#[allow(clippy::too_many_arguments)] // reaper config knobs, all primitives
fn reaper_loop(
    inner: Arc<RwLock<Inner>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
    samples: usize,
    rounds: u32,
    rewrite_pct: u32,
    rewrite_min_size: u64,
    sink: Option<MetricSink>,
) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        {
            let mut g = lock_inner(&inner);
            let _ = g.store.tick_expire(samples, rounds);
            // EverySec AOF fsync window check — embedded mode runs this from
            // the same reaper tick rather than a separate timer.
            if let Some(aof) = &mut g.aof {
                let _ = aof.maybe_sync();
            }
        }
        // Non-blocking: holds the lock only for begin/finish, not the spill.
        concurrent_auto_rewrite(&inner, rewrite_pct, rewrite_min_size, sink.as_ref());
    }
}

/// Has the AOF grown `pct` percent past its size at the last rewrite and is it
/// at least `min_size` bytes? (Redis's `auto-aof-rewrite-percentage` /
/// `-min-size`.) `pct == 0` always returns false (auto-rewrite disabled).
fn rewrite_threshold_met(aof: &Aof, pct: u32, min_size: u64) -> bool {
    if pct == 0 || aof.is_rewriting() {
        return false;
    }
    let cur = aof.size_bytes();
    if cur < min_size {
        return false;
    }
    let baseline = aof.size_at_last_rewrite().max(1);
    // (cur - baseline) * 100 / baseline ≥ pct  ⇔  cur * 100 ≥ baseline * (100 + pct)
    cur.saturating_mul(100) >= baseline.saturating_mul(100u64.saturating_add(pct as u64))
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
pub(crate) fn concurrent_auto_rewrite(
    inner: &Arc<RwLock<Inner>>,
    pct: u32,
    min_size: u64,
    sink: Option<&MetricSink>,
) {
    let start = Instant::now();
    // Phase 1 — decide + begin, under the lock.
    let (plan, before_bytes) = {
        let mut g = lock_inner(inner);
        let ready = g.aof.as_ref().is_some_and(|a| rewrite_threshold_met(a, pct, min_size));
        if !ready {
            return;
        }
        let Inner { store, aof, .. } = &mut *g;
        let aof = aof.as_mut().expect("checked above");
        let before = aof.size_bytes();
        match aof.begin_concurrent_rewrite(store) {
            Ok(p) => (p, before),
            Err(e) => {
                eprintln!("kevy: embedded auto AOF rewrite (begin) failed: {e}");
                return;
            }
        }
    };
    // Phase 2 — spill the snapshot image to disk, lock released.
    if let Err(e) = spill_rewrite_body(&plan.tmp, &plan.body) {
        eprintln!("kevy: embedded auto AOF rewrite (spill) failed: {e}");
        let mut g = lock_inner(inner);
        if let Some(aof) = &mut g.aof {
            aof.abort_concurrent_rewrite();
        }
        let _ = std::fs::remove_file(&plan.tmp);
        return;
    }
    // Phase 3 — append the diff, swap, reopen, under the lock.
    let mut g = lock_inner(inner);
    let Some(aof) = &mut g.aof else { return };
    match aof.finish_concurrent_rewrite(&plan.tmp, plan.keys) {
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
            let _ = std::fs::remove_file(&plan.tmp);
        }
    }
}

/// Write the rewrite snapshot image to `tmp` and fsync it. Pure I/O; runs
/// with the store lock released.
fn spill_rewrite_body(tmp: &std::path::Path, body: &[u8]) -> io::Result<()> {
    let mut f = std::fs::File::create(tmp)?;
    f.write_all(body)?;
    f.sync_all()
}

/// Write-lock the inner state, recovering from a poisoned lock (a method panic
/// elsewhere left data intact in memory). The reaper mutates (reap + clock
/// refresh + rewrite), so it always takes the write side.
pub(crate) fn lock_inner(inner: &Arc<RwLock<Inner>>) -> RwLockWriteGuard<'_, Inner> {
    inner.write().unwrap_or_else(|p| p.into_inner())
}
