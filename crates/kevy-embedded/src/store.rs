//! [`Store`] — the embedded entry point. Wraps `kevy_store::Store` with
//! a mutex (for cross-thread access), optional AOF auto-logging, an
//! optional background TTL reaper, and an in-process pub/sub bus.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use kevy_persist::{Aof, Argv, RewriteStats, load_snapshot, replay_aof, save_snapshot};
use kevy_store::{ExpireStats, StoreError};

use crate::config::{Config, TtlReaperMode};
use crate::pubsub::PubsubBus;

/// The embedded keyspace.
///
/// **`Store` is `Clone`** (since v1.1.0). A clone is a cheap `Arc` bump:
/// every clone reaches the same underlying `kevy_store::Store` + AOF +
/// reaper + pub/sub bus. The reaper thread is joined and the AOF is
/// flushed exactly once, when the **last** clone is dropped.
///
/// ```
/// use kevy_embedded::{Config, Store};
///
/// # fn main() -> std::io::Result<()> {
/// let s = Store::open(Config::default().with_ttl_reaper_manual())?;
/// let s2 = s.clone();
/// std::thread::spawn(move || {
///     s2.set(b"from-thread", b"v").unwrap();
/// }).join().unwrap();
/// assert_eq!(s.get(b"from-thread")?, Some(b"v".to_vec()));
/// # Ok(())
/// # }
/// ```
///
/// Every method takes `&self`. The internal `Arc<Mutex<Inner>>` is what
/// makes shared access safe under contention.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
    /// Shared drop guard: signals + joins reaper and flushes AOF when the
    /// LAST `Store` clone (or `Subscription`) holding a strong ref drops.
    guard: Arc<DropGuard>,
    config: Config,
}

/// Weak handle to a `Store` — does not keep the underlying keyspace alive.
///
/// Used by the URL-keyed registry in `kevy-client` so that multiple
/// `Connection::open("mem://name")` calls share the same backing store
/// without leaking it when all strong handles go away.
pub struct WeakStore {
    inner: Weak<Mutex<Inner>>,
    guard: Weak<DropGuard>,
    config: Config,
}

impl WeakStore {
    /// Try to upgrade back to a `Store`. Returns `None` if the last strong
    /// reference has already been dropped.
    pub fn upgrade(&self) -> Option<Store> {
        Some(Store {
            inner: self.inner.upgrade()?,
            guard: self.guard.upgrade()?,
            config: self.config.clone(),
        })
    }
}

pub(crate) struct Inner {
    pub(crate) store: kevy_store::Store,
    pub(crate) aof: Option<Aof>,
    pub(crate) bus: PubsubBus,
}

/// Owns the reaper-thread handle + a back-reference to `Inner` for the
/// final AOF flush. Lives in an `Arc<DropGuard>` shared across every
/// `Store` clone; the actual drop logic fires only when the last clone
/// goes away. `JoinHandle` is wrapped in `Mutex<Option>` so `Drop` can
/// `.take()` it while only having `&self`.
pub(crate) struct DropGuard {
    reaper_stop: Option<Arc<AtomicBool>>,
    reaper_join: Mutex<Option<JoinHandle<()>>>,
    inner_for_flush: Arc<Mutex<Inner>>,
}

impl Store {
    /// Open an embedded keyspace per `config`.
    ///
    /// - Pure in-memory when `config.data_dir` is `None`.
    /// - With persistence: loads `<data_dir>/<snapshot_filename>` first,
    ///   then replays `<data_dir>/<aof_filename>`. Both are best-effort —
    ///   missing files are fine, a truncated AOF tail is silently dropped.
    /// - Spawns a background TTL reaper thread when
    ///   `config.ttl_reaper == Background` (the default).
    pub fn open(config: Config) -> io::Result<Self> {
        let (store, aof) = init_persistent_store(&config)?;
        let inner = Arc::new(Mutex::new(Inner {
            store,
            aof,
            bus: PubsubBus::new(),
        }));
        let (reaper_stop, reaper_join) = spawn_reaper(&config, &inner)?;
        let guard = Arc::new(DropGuard {
            reaper_stop,
            reaper_join: Mutex::new(reaper_join),
            inner_for_flush: inner.clone(),
        });
        Ok(Store {
            inner,
            guard,
            config,
        })
    }

    /// Get a weak handle that does not keep the keyspace alive.
    /// `upgrade()` returns `None` once the last strong `Store` is dropped.
    pub fn downgrade(&self) -> WeakStore {
        WeakStore {
            inner: Arc::downgrade(&self.inner),
            guard: Arc::downgrade(&self.guard),
            config: self.config.clone(),
        }
    }

    /// The active config (a clone — modifying it has no effect on the
    /// running store). Useful for introspection / `INFO`-style telemetry.
    pub fn config(&self) -> &Config {
        &self.config
    }

    // ---- escape hatches -------------------------------------------------

    /// Run `f` against the underlying `kevy_store::Store` under the
    /// embedded mutex. Use for direct access to methods this crate hasn't
    /// wrapped (snapshot iteration, ZRANGE, raw collect_keys, …). The
    /// closure can mutate, but *does not auto-log to the AOF* — call
    /// [`Self::log`] yourself if the mutation must survive a crash.
    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut kevy_store::Store) -> R,
    {
        let mut g = self.lock();
        f(&mut g.store)
    }

    /// Append a raw RESP-frame argument list to the AOF. Pairs with
    /// [`Self::with`] when the closure performed a write you want to make
    /// crash-safe. No-op when persistence is disabled.
    pub fn log(&self, parts: &[&[u8]]) -> io::Result<()> {
        let mut g = self.lock();
        if let Some(aof) = &mut g.aof {
            let argv = Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
            aof.append(&argv)?;
        }
        Ok(())
    }

    // ---- maintenance ----------------------------------------------------

    /// Run one TTL-reaper tick. Required call cadence in `Manual` mode
    /// (call ~10× per second to match Redis's `hz=10`); no-op cost is
    /// one mutex lock + map-emptiness check when nothing has TTL.
    pub fn tick(&self) -> ExpireStats {
        let mut g = self.lock();
        let stats = g
            .store
            .tick_expire(self.config.reaper_samples, self.config.reaper_max_rounds);
        // Auto-AOF-rewrite check rides the same caller-driven tick in Manual
        // mode (Background mode runs it from the reaper thread instead).
        maybe_auto_rewrite(
            &mut g,
            self.config.auto_aof_rewrite_pct,
            self.config.auto_aof_rewrite_min_size,
        );
        stats
    }

    /// `BGREWRITEAOF`: rebuild the AOF from current state. Synchronous —
    /// blocks until the rewrite + atomic rename completes. Returns
    /// `Ok(None)` when persistence is disabled.
    pub fn rewrite_aof(&self) -> io::Result<Option<RewriteStats>> {
        let mut g = self.lock();
        // Disjoint-field split-borrow: destructure the guard so the borrow
        // checker sees `store` and `aof` as independent borrows, not two
        // claims on the same `&mut Inner`.
        let Inner { store, aof, bus: _ } = &mut *g;
        let Some(aof) = aof else { return Ok(None) };
        Ok(Some(aof.rewrite_from(store)?))
    }

    /// Snapshot the store to `<data_dir>/<snapshot_filename>`, atomically.
    /// `Ok(false)` when persistence is disabled (caller can decide to
    /// surface that or no-op).
    pub fn save_snapshot(&self) -> io::Result<bool> {
        let g = self.lock();
        let Some(dir) = self.config.data_dir.as_ref() else {
            return Ok(false);
        };
        let path: PathBuf = dir.join(&self.config.snapshot_filename);
        save_snapshot(&g.store, &path)?;
        Ok(true)
    }

    // String / hash / list / set / zset / pub-sub data-type methods live
    // in `crate::ops` (kept under the 500-LOC file cap). Look there for
    // e.g. `Store::set` / `Store::hset` / `Store::publish`.

    /// Crate-internal: clone the shared `Arc<Mutex<Inner>>` handle, used
    /// by `ops.rs::Store::subscribe` to hand the bus to a `Subscription`.
    pub(crate) fn inner_handle(&self) -> Arc<Mutex<Inner>> {
        self.inner.clone()
    }

    /// Crate-internal: clone the shared `Arc<DropGuard>` so a live
    /// `Subscription` keeps the reaper + AOF flush alive until it drops.
    pub(crate) fn guard_handle(&self) -> Arc<DropGuard> {
        self.guard.clone()
    }

    /// Crate-internal: acquire the embedded mutex. Recovers from poison
    /// because every method's critical section is short — a panic in one
    /// doesn't corrupt the keyspace.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        }
    }
}

/// Build the `kevy_store::Store` and (optionally) its `Aof`. Loads any
/// pre-existing snapshot and replays any pre-existing AOF before
/// returning. `data_dir = None` ⇒ pure in-memory (both return values
/// are the empty store + `None`).
fn init_persistent_store(config: &Config) -> io::Result<(kevy_store::Store, Option<Aof>)> {
    let mut store = kevy_store::Store::new();
    store.set_max_memory(config.maxmemory, config.eviction_policy);
    let aof = if let Some(dir) = &config.data_dir {
        std::fs::create_dir_all(dir)?;
        let snap_path = dir.join(&config.snapshot_filename);
        if snap_path.exists() {
            load_snapshot(&mut store, &snap_path)?;
        }
        let aof_path = dir.join(&config.aof_filename);
        if aof_path.exists() {
            replay_aof(&aof_path, |args| crate::replay::apply(&mut store, &args))?;
        }
        if config.aof {
            Some(Aof::open(&aof_path, config.appendfsync)?)
        } else {
            None
        }
    } else {
        None
    };
    Ok((store, aof))
}

/// Start the background TTL reaper thread, returning its stop signal +
/// join handle. `TtlReaperMode::Manual` returns `(None, None)` so the
/// caller-driven reap is in charge instead.
#[allow(clippy::type_complexity)] // inline tuple keeps the pair colocated
fn spawn_reaper(
    config: &Config,
    inner: &Arc<Mutex<Inner>>,
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
            let handle = std::thread::Builder::new()
                .name(String::from("kevy-embedded-reaper"))
                .spawn(move || {
                    reaper_loop(inner_t, stop_t, interval, samples, rounds, rw_pct, rw_min)
                })?;
            Ok((Some(stop), Some(handle)))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// DELETION MARKER (replaced below): everything from the original `set`
// method through the closing brace of `impl Store` previously lived here.
// They've been moved to `crate::ops` — see crates/kevy-embedded/src/ops.rs.
// ─────────────────────────────────────────────────────────────────────────

fn log_argv(aof: &mut Option<Aof>, parts: &[&[u8]]) -> io::Result<()> {
    if let Some(aof) = aof {
        let argv = Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
        aof.append(&argv)?;
    }
    Ok(())
}

/// Complete a write: AOF-log the canonical RESP command, then run the
/// store's post-write eviction sweep. Single helper so every write wrapper
/// stays in lockstep — forgetting to evict means a maxmemory budget would
/// grow without bound.
pub(crate) fn commit_write(inner: &mut Inner, parts: &[&[u8]]) -> io::Result<()> {
    log_argv(&mut inner.aof, parts)?;
    inner.store.try_evict_after_write();
    Ok(())
}

pub(crate) fn store_err(e: StoreError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("kevy-store: {e:?}"))
}

#[allow(clippy::too_many_arguments)] // reaper config knobs, all primitives
fn reaper_loop(
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
    samples: usize,
    rounds: u32,
    rewrite_pct: u32,
    rewrite_min_size: u64,
) {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut g = match inner.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        };
        let _ = g.store.tick_expire(samples, rounds);
        // EverySec AOF fsync window check — embedded mode runs this from
        // the same reaper tick rather than a separate timer.
        if let Some(aof) = &mut g.aof {
            let _ = aof.maybe_sync();
        }
        maybe_auto_rewrite(&mut g, rewrite_pct, rewrite_min_size);
    }
}

/// Auto-`BGREWRITEAOF`: rewrite the AOF when it has grown `pct` percent past
/// its size at the last rewrite and is at least `min_size` bytes (Redis's
/// `auto-aof-rewrite-percentage` / `-min-size`). Mirrors the server runtime's
/// `Shard::maybe_auto_rewrite_aof`. Runs under the held `Inner` lock, so it
/// briefly blocks writers while it rewrites — acceptable because it fires
/// rarely (only when the AOF has doubled past the floor). `pct == 0` disables.
pub(crate) fn maybe_auto_rewrite(g: &mut Inner, pct: u32, min_size: u64) {
    if pct == 0 {
        return;
    }
    let Inner { store, aof, .. } = g;
    let Some(aof) = aof else { return };
    let cur = aof.size_bytes();
    if cur < min_size {
        return;
    }
    let baseline = aof.size_at_last_rewrite().max(1);
    // (cur - baseline) * 100 / baseline ≥ pct  ⇔  cur * 100 ≥ baseline * (100 + pct)
    if cur.saturating_mul(100) < baseline.saturating_mul(100u64.saturating_add(pct as u64)) {
        return;
    }
    if let Err(e) = aof.rewrite_from(store) {
        eprintln!("kevy: embedded auto AOF rewrite failed: {e}");
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Last `Store` clone is going away — stop the reaper, join it, then
        // flush the AOF so EverySec users don't lose the last sub-second of
        // writes. Poison recovery: a method panic earlier shouldn't strand
        // the AOF unflushed; the writes already landed in-memory.
        if let Some(stop) = &self.reaper_stop {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(j) = self
            .reaper_join
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = j.join();
        }
        let mut g = match self.inner_for_flush.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        };
        if let Some(aof) = &mut g.aof {
            let _ = aof.maybe_sync();
        }
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
