//! [`Store`] — the embedded entry point. Wraps `kevy_store::Store` with
//! per-shard locks (for cross-thread access), optional AOF auto-logging, an
//! optional background TTL reaper, and an in-process pub/sub bus.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::metric::KevyMetric;

use kevy_persist::{Aof, Argv, RewriteStats, save_snapshot};
use kevy_store::{ExpireStats, StoreError};

use crate::config::Config;
use crate::pubsub::PubsubBus;
use crate::shard::{build_shards, shard_idx};

/// The keyspace shards (`hash(key) % n`), each a fully independent
/// `kevy_store::Store` + AOF behind its own lock. `n == 1` (the default) is a
/// one-element vec = the original single-lock store.
pub(crate) type Shards = Arc<Vec<Arc<RwLock<Inner>>>>;

/// The embedded keyspace.
///
/// **`Store` is `Clone`** (since v1.1.0). A clone is a cheap `Arc` bump:
/// every clone reaches the same underlying shards + AOF + reaper + pub/sub
/// bus. The reaper thread is joined and each shard's AOF is flushed exactly
/// once, when the **last** clone is dropped.
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
/// Every method takes `&self`. Sharding (see [`Config::with_shards`]) lets a
/// multi-threaded consumer scale across cores; pub/sub is process-wide
/// (handled on shard 0).
#[derive(Clone)]
pub struct Store {
    shards: Shards,
    /// Shared drop guard: signals + joins reaper and flushes AOFs when the
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
    shards: Weak<Vec<Arc<RwLock<Inner>>>>,
    guard: Weak<DropGuard>,
    config: Config,
}

impl WeakStore {
    /// Try to upgrade back to a `Store`. Returns `None` if the last strong
    /// reference has already been dropped.
    pub fn upgrade(&self) -> Option<Store> {
        Some(Store {
            shards: self.shards.upgrade()?,
            guard: self.guard.upgrade()?,
            config: self.config.clone(),
        })
    }
}

pub(crate) struct Inner {
    pub(crate) store: kevy_store::Store,
    pub(crate) aof: Option<Aof>,
    /// Pub/sub bus. Only shard 0's is ever used (pub/sub is process-wide);
    /// other shards carry an idle one (cheap).
    pub(crate) bus: PubsubBus,
}

impl Inner {
    pub(crate) fn new(store: kevy_store::Store, aof: Option<Aof>) -> Self {
        Inner { store, aof, bus: PubsubBus::new() }
    }
}

/// Owns the reaper-thread handle + the shards for the final AOF flush. Lives
/// in an `Arc<DropGuard>` shared across every `Store` clone; the drop logic
/// fires only when the last clone goes away.
pub(crate) struct DropGuard {
    reaper_stop: Option<Arc<AtomicBool>>,
    reaper_join: Mutex<Option<JoinHandle<()>>>,
    shards_for_flush: Shards,
}

impl Store {
    /// Open an embedded keyspace per `config`.
    ///
    /// - Pure in-memory when `config.data_dir` is `None`.
    /// - With persistence: each shard loads its snapshot then replays its AOF
    ///   (`config.shards > 1` re-shards a legacy single AOF on first open).
    /// - Spawns a background TTL reaper thread when
    ///   `config.ttl_reaper == Background` (the default).
    pub fn open(config: Config) -> io::Result<Self> {
        let shards: Shards = Arc::new(build_shards(&config)?);
        let (reaper_stop, reaper_join) = crate::reaper::spawn_reaper(&config, &shards)?;
        let guard = Arc::new(DropGuard {
            reaper_stop,
            reaper_join: Mutex::new(reaper_join),
            shards_for_flush: shards.clone(),
        });
        Ok(Store { shards, guard, config })
    }

    /// Get a weak handle that does not keep the keyspace alive.
    pub fn downgrade(&self) -> WeakStore {
        WeakStore {
            shards: Arc::downgrade(&self.shards),
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

    /// Run `f` against the underlying `kevy_store::Store` under its lock. Use
    /// for direct access to methods this crate hasn't wrapped. The closure can
    /// mutate, but *does not auto-log to the AOF* — call [`Self::log`] yourself
    /// if the mutation must survive a crash.
    ///
    /// **Sharded stores:** this targets shard 0 only. Use [`Self::with_key`]
    /// to reach the shard owning a specific key.
    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut kevy_store::Store) -> R,
    {
        let mut g = self.lock();
        f(&mut g.store)
    }

    /// Like [`Self::with`] but targets the shard that owns `key`.
    pub fn with_key<F, R>(&self, key: &[u8], f: F) -> R
    where
        F: FnOnce(&mut kevy_store::Store) -> R,
    {
        let mut g = self.wshard(key);
        f(&mut g.store)
    }

    /// Append a raw RESP-frame argument list to the shard owning its key's
    /// AOF. No-op when persistence is disabled.
    pub fn log(&self, parts: &[&[u8]]) -> io::Result<()> {
        let mut g = match parts.get(1) {
            Some(key) => self.wshard(key),
            None => self.lock(),
        };
        if let Some(aof) = &mut g.aof {
            let argv = Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
            aof.append(&argv)?;
        }
        Ok(())
    }

    // ---- maintenance ----------------------------------------------------

    /// Run one TTL-reaper tick across every shard. Required call cadence in
    /// `Manual` mode (~10×/s to match Redis `hz=10`). Returns the summed stats.
    pub fn tick(&self) -> ExpireStats {
        let mut total = ExpireStats::default();
        for shard in self.shards.iter() {
            let stats = {
                let mut g = lock_write(shard);
                g.store.tick_expire(self.config.reaper_samples, self.config.reaper_max_rounds)
            };
            total.sampled += stats.sampled;
            total.expired += stats.expired;
            // Auto-rewrite rides the caller-driven tick in Manual mode; the
            // non-blocking path releases the lock for the disk spill.
            crate::reaper::concurrent_auto_rewrite(
                shard,
                self.config.auto_aof_rewrite_pct,
                self.config.auto_aof_rewrite_min_size,
                self.config.metric_sink.as_ref(),
            );
        }
        total
    }

    /// `BGREWRITEAOF`: rebuild every shard's AOF from current state.
    /// Synchronous. Returns the summed stats (`None` if persistence is off /
    /// no shard rewrote).
    pub fn rewrite_aof(&self) -> io::Result<Option<RewriteStats>> {
        let mut agg: Option<RewriteStats> = None;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let Inner { store, aof, bus: _ } = &mut *g;
            let Some(aof) = aof else { continue };
            if aof.is_rewriting() {
                continue;
            }
            let before_bytes = aof.size_bytes();
            let start = Instant::now();
            let stats = aof.rewrite_from(store)?;
            if let Some(sink) = &self.config.metric_sink {
                sink.emit(KevyMetric::Rewrite {
                    keys: stats.keys,
                    before_bytes,
                    after_bytes: stats.bytes,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
            let acc = agg.get_or_insert(RewriteStats { keys: 0, bytes: 0 });
            acc.keys += stats.keys;
            acc.bytes += stats.bytes;
        }
        Ok(agg)
    }

    /// Snapshot every shard to its `dump-{i}.rdb` (single shard: the configured
    /// name), atomically. `Ok(false)` when persistence is disabled.
    pub fn save_snapshot(&self) -> io::Result<bool> {
        let Some(dir) = self.config.data_dir.as_ref() else {
            return Ok(false);
        };
        let n = self.shards.len();
        for (i, shard) in self.shards.iter().enumerate() {
            let name = if n == 1 {
                self.config.snapshot_filename.clone()
            } else {
                format!("dump-{i}.rdb")
            };
            let g = lock_read(shard);
            save_snapshot(&g.store, &dir.join(name))?;
        }
        Ok(true)
    }

    // Data-type methods live in `crate::ops` / `crate::info`.

    /// Crate-internal: clone shard 0's handle for a `Subscription`'s bus.
    pub(crate) fn inner_handle(&self) -> Arc<RwLock<Inner>> {
        self.shards[0].clone()
    }

    /// Crate-internal: clone the shared `Arc<DropGuard>`.
    pub(crate) fn guard_handle(&self) -> Arc<DropGuard> {
        self.guard.clone()
    }

    fn shard_for(&self, key: &[u8]) -> &Arc<RwLock<Inner>> {
        &self.shards[shard_idx(key, self.shards.len())]
    }

    /// Write-lock the shard owning `key`.
    pub(crate) fn wshard(&self, key: &[u8]) -> RwLockWriteGuard<'_, Inner> {
        lock_write(self.shard_for(key))
    }

    /// Read-lock the shard owning `key` (GET fast path — concurrent readers
    /// across shards run in parallel).
    pub(crate) fn rshard(&self, key: &[u8]) -> RwLockReadGuard<'_, Inner> {
        lock_read(self.shard_for(key))
    }

    /// Write-lock shard 0 — pub/sub bus + keyless escape hatches.
    pub(crate) fn lock(&self) -> RwLockWriteGuard<'_, Inner> {
        lock_write(&self.shards[0])
    }

    /// Run `f` over every shard's write guard, summing a `usize` (DBSIZE etc.).
    pub(crate) fn sum_shards<F: Fn(&mut Inner) -> usize>(&self, f: F) -> usize {
        self.shards.iter().map(|s| f(&mut lock_write(s))).sum()
    }

    /// Run `f` over every shard's write guard, summing a `u64`.
    pub(crate) fn sum_shards_u64<F: Fn(&mut Inner) -> u64>(&self, f: F) -> u64 {
        self.shards.iter().map(|s| f(&mut lock_write(s))).sum()
    }

    /// Run a fallible `f` over every shard (mutating, e.g. FLUSHALL).
    pub(crate) fn try_for_each_shard<F: FnMut(&mut Inner) -> io::Result<()>>(
        &self,
        mut f: F,
    ) -> io::Result<()> {
        for s in self.shards.iter() {
            f(&mut lock_write(s))?;
        }
        Ok(())
    }
}

/// Write-lock an `Inner`, recovering from poison (short critical sections; a
/// panic in one doesn't corrupt the keyspace).
pub(crate) fn lock_write(shard: &RwLock<Inner>) -> RwLockWriteGuard<'_, Inner> {
    shard.write().unwrap_or_else(|p| p.into_inner())
}

/// Read-lock an `Inner`, recovering from poison.
pub(crate) fn lock_read(shard: &RwLock<Inner>) -> RwLockReadGuard<'_, Inner> {
    shard.read().unwrap_or_else(|p| p.into_inner())
}

fn log_argv(aof: &mut Option<Aof>, parts: &[&[u8]]) -> io::Result<()> {
    if let Some(aof) = aof {
        let argv = Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
        aof.append(&argv)?;
    }
    Ok(())
}

/// Complete a write on one shard: AOF-log the canonical RESP command, then run
/// that shard's post-write eviction sweep.
pub(crate) fn commit_write(inner: &mut Inner, parts: &[&[u8]]) -> io::Result<()> {
    log_argv(&mut inner.aof, parts)?;
    inner.store.try_evict_after_write();
    Ok(())
}

pub(crate) fn store_err(e: StoreError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("kevy-store: {e:?}"))
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Stop + join the reaper, then flush every shard's AOF so EverySec
        // users don't lose the last sub-second of writes.
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
        for shard in self.shards_for_flush.iter() {
            let mut g = lock_write(shard);
            if let Some(aof) = &mut g.aof {
                let _ = aof.maybe_sync();
            }
        }
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
