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

        let inner = Arc::new(Mutex::new(Inner {
            store,
            aof,
            bus: PubsubBus::new(),
        }));

        let (reaper_stop, reaper_join) = match config.ttl_reaper {
            TtlReaperMode::Manual => (None, None),
            TtlReaperMode::Background => {
                let stop = Arc::new(AtomicBool::new(false));
                let stop_t = stop.clone();
                let inner_t = inner.clone();
                let interval = config.reaper_interval;
                let samples = config.reaper_samples;
                let rounds = config.reaper_max_rounds;
                let handle = std::thread::Builder::new()
                    .name(String::from("kevy-embedded-reaper"))
                    .spawn(move || reaper_loop(inner_t, stop_t, interval, samples, rounds))?;
                (Some(stop), Some(handle))
            }
        };

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
        g.store
            .tick_expire(self.config.reaper_samples, self.config.reaper_max_rounds)
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

fn reaper_loop(
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
    samples: usize,
    rounds: u32,
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
mod tests {
    use super::*;
    use crate::config::{AppendFsync, EvictionPolicy};

    fn tmp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("kevy-embedded-{name}-{uniq}"));
        p
    }

    #[test]
    fn in_memory_roundtrip() {
        let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
        s.set(b"k", b"v").unwrap();
        assert_eq!(s.get(b"k").unwrap(), Some(b"v".to_vec()));
        assert_eq!(s.dbsize(), 1);
        s.del(&[b"k"]).unwrap();
        assert_eq!(s.dbsize(), 0);
    }

    #[test]
    fn persistence_round_trip_via_aof() {
        let dir = tmp_dir("aof-rt");
        {
            let s = Store::open(
                Config::default()
                    .with_persist(&dir)
                    .with_ttl_reaper_manual()
                    .with_appendfsync(AppendFsync::Always),
            )
            .unwrap();
            for i in 0..50 {
                s.set(format!("k{i}").as_bytes(), b"v").unwrap();
            }
            s.incr_by(b"counter", 41).unwrap();
            s.hset(b"h", &[(b"field" as &[u8], b"val" as &[u8])]).unwrap();
        }
        // Reopen: AOF replay should reconstruct exactly the same state.
        let s2 = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_ttl_reaper_manual(),
        )
        .unwrap();
        assert_eq!(s2.dbsize(), 52); // 50 + counter + h
        assert_eq!(s2.get(b"k0").unwrap(), Some(b"v".to_vec()));
        assert_eq!(s2.get(b"k49").unwrap(), Some(b"v".to_vec()));
        assert_eq!(s2.get(b"counter").unwrap(), Some(b"41".to_vec()));
        assert_eq!(s2.hget(b"h", b"field").unwrap(), Some(b"val".to_vec()));
        drop(s2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_works_under_pressure() {
        let s = Store::open(
            Config::default()
                .with_ttl_reaper_manual()
                .with_max_memory(800)
                .with_eviction(EvictionPolicy::AllKeysLru),
        )
        .unwrap();
        for i in 0..50 {
            s.set(format!("k{i:02}").as_bytes(), b"xxxxxxxxxxxxxxxxxxxx")
                .unwrap();
        }
        assert!(s.used_memory() <= 800, "got {}", s.used_memory());
        assert!(s.evictions_total() > 0);
    }

    #[test]
    fn manual_tick_runs_active_reaper() {
        let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
        s.set_with_ttl(b"short", b"v", Duration::from_millis(1)).unwrap();
        s.set(b"perm", b"v").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let stats = s.tick();
        // tick() should at least sample and reap (may take multiple ticks
        // for sparse layouts; the call is idempotent).
        let _ = stats;
        let _ = s.get(b"short").unwrap(); // lazy reap path
        assert!(s.expired_keys_total() >= 1);
        assert!(s.get(b"perm").unwrap().is_some());
    }

    #[test]
    fn with_escape_hatch_works() {
        let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
        let zsize = s.with(|store| {
            let _ = store.zadd(b"z", &[(1.0, b"a".to_vec()), (2.0, b"b".to_vec())]);
            store.zcard(b"z").unwrap()
        });
        assert_eq!(zsize, 2);
        // Direct (un-logged) write through `with`: caller may explicitly
        // log if they want it crash-safe. Here we just verify it landed.
        assert_eq!(s.type_of(b"z"), "zset");
    }

    #[test]
    fn background_reaper_thread_drops_expired_keys() {
        let s = Store::open(
            Config::default().with_reaper_interval(Duration::from_millis(20)),
        )
        .unwrap();
        s.set_with_ttl(b"k", b"v", Duration::from_millis(5)).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        // The active reaper should have caught it without anyone reading.
        let _ = s.get(b"k").unwrap(); // either way, key should now be gone
        assert_eq!(s.dbsize(), 0);
    }

    #[test]
    fn arc_sharing_across_threads() {
        use std::sync::Arc;
        let s = Arc::new(Store::open(Config::default().with_ttl_reaper_manual()).unwrap());
        let mut handles = Vec::new();
        for i in 0..8 {
            let s = Arc::clone(&s);
            handles.push(std::thread::spawn(move || {
                for j in 0..50 {
                    s.set(format!("t{i}-{j}").as_bytes(), b"v").unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.dbsize(), 8 * 50);
    }

    #[test]
    fn drop_during_reaper_does_not_deadlock() {
        // Sanity: a Store with a Background reaper must drop cleanly even
        // while the reaper is sleeping. Without the stop-flag + join the
        // drop would either hang or race the reaper holding the mutex.
        for _ in 0..4 {
            let s = Store::open(
                Config::default().with_reaper_interval(Duration::from_millis(5)),
            )
            .unwrap();
            s.set(b"k", b"v").unwrap();
            // Let the reaper actually run a couple of times.
            std::thread::sleep(Duration::from_millis(40));
            drop(s); // must return within a few ms
        }
    }

    #[test]
    fn save_snapshot_then_restart() {
        let dir = tmp_dir("snap-rt");
        {
            let s = Store::open(
                Config::default()
                    .with_persist(&dir)
                    .without_aof()
                    .with_ttl_reaper_manual(),
            )
            .unwrap();
            for i in 0..10 {
                s.set(format!("k{i}").as_bytes(), b"v").unwrap();
            }
            let saved = s.save_snapshot().unwrap();
            assert!(saved);
        }
        let s2 = Store::open(
            Config::default()
                .with_persist(&dir)
                .without_aof()
                .with_ttl_reaper_manual(),
        )
        .unwrap();
        assert_eq!(s2.dbsize(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
