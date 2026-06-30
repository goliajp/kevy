//! [`Store`] — the embedded entry point. Wraps `kevy_store::Store` with
//! per-shard locks (for cross-thread access), optional AOF auto-logging, an
//! optional background TTL reaper, and an in-process pub/sub bus.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::thread::JoinHandle;

use kevy_persist::{Aof, Argv};
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
    pub(crate) shards: Shards,
    /// Shared drop guard: signals + joins reaper and flushes AOFs when the
    /// LAST `Store` clone (or `Subscription`) holding a strong ref drops.
    pub(crate) guard: Arc<DropGuard>,
    pub(crate) config: Config,
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
    /// Phase 3 / v1.21: shared replication source if this store is
    /// an embed-as-writer. Every shard holds a clone of the same
    /// `Arc<Mutex<...>>` so `commit_write` can push mutations
    /// without reaching back up through the `DropGuard`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) writer_source:
        Option<std::sync::Arc<Mutex<kevy_replicate::source::ReplicationSource>>>,
}

impl Inner {
    pub(crate) fn new(store: kevy_store::Store, aof: Option<Aof>) -> Self {
        Inner {
            store,
            aof,
            bus: PubsubBus::new(),
            #[cfg(not(target_arch = "wasm32"))]
            writer_source: None,
        }
    }
}

/// Owns the reaper-thread handle + the shards for the final AOF flush. Lives
/// in an `Arc<DropGuard>` shared across every `Store` clone; the drop logic
/// fires only when the last clone goes away.
pub(crate) struct DropGuard {
    reaper_stop: Option<Arc<AtomicBool>>,
    reaper_join: Mutex<Option<JoinHandle<()>>>,
    shards_for_flush: Shards,
    /// Replica runner thread + reconnect machinery, present iff this
    /// store was opened with `Config::replica_upstream = Some(...)`.
    /// Joined here so the runner stops cleanly when the last `Store`
    /// clone goes away.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) replica_runner: Option<crate::replica_runner::ReplicaRunner>,
    /// Replica-source listener + accepted connection threads, present
    /// iff this store is a Phase 3 embed-as-writer
    /// (`Config::embed_writer_listen_addr = Some(...)`). Joined on
    /// last-clone drop.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) replica_source: Option<crate::replica_source::ReplicaSource>,
}

impl Store {
    /// Open an embedded keyspace per `config`.
    ///
    /// - Pure in-memory when `config.data_dir` is `None`.
    /// - With persistence: each shard loads its snapshot then replays its AOF
    ///   (`config.shards > 1` re-shards a legacy single AOF on first open).
    /// - Spawns a background TTL reaper thread when
    ///   `config.ttl_reaper == Background` (the default).
    /// - When `config.replica_upstream = Some("host:port")`, spawns a
    ///   background thread that streams replication frames from the
    ///   named primary and applies them to this store; local writes are
    ///   rejected with `READONLY` (see [`Self::open_replica`]).
    pub fn open(config: Config) -> io::Result<Self> {
        let shards: Shards = Arc::new(build_shards(&config)?);
        let (reaper_stop, reaper_join) = crate::reaper::spawn_reaper(&config, &shards)?;
        #[cfg(not(target_arch = "wasm32"))]
        let replica_runner = crate::replica_glue::spawn_replica_runner(&config, &shards);
        #[cfg(not(target_arch = "wasm32"))]
        let replica_source = match config.embed_writer_listen_addr.as_ref() {
            Some(addr) => {
                let rs = crate::replica_source::ReplicaSource::spawn(
                    addr,
                    config.embed_writer_backlog_bytes,
                )?;
                // Inject the shared source Arc into every shard's
                // Inner so `commit_write` pushes mutations into the
                // backlog inline. Done once at open under the
                // shard's write lock; reads of `Inner::writer_source`
                // afterwards are uncontended.
                let shared = rs.shared_source();
                for shard in shards.iter() {
                    let mut g = lock_write(shard);
                    g.writer_source = Some(shared.clone());
                }
                Some(rs)
            }
            None => None,
        };
        let guard = Arc::new(DropGuard {
            reaper_stop,
            reaper_join: Mutex::new(reaper_join),
            shards_for_flush: shards.clone(),
            #[cfg(not(target_arch = "wasm32"))]
            replica_runner,
            #[cfg(not(target_arch = "wasm32"))]
            replica_source,
        });
        Ok(Store { shards, guard, config })
    }

    /// Convenience constructor for an embed-as-read-replica store
    /// streaming writes from `upstream` (`"host:port"` of a kevy
    /// server's replication listener).
    ///
    /// The replica:
    /// - has its local AOF force-disabled (the upstream stream is the
    ///   source of truth; replica AOF would diverge and double-apply
    ///   on restart);
    /// - rejects every local write with a `READONLY` `io::Error`
    ///   (you can still call read APIs concurrently);
    /// - reconnects with exponential backoff on disconnect, resuming
    ///   from the last applied offset;
    /// - gets a process-unique `replica_id` so an open / drop / reopen
    ///   cycle within the primary's reconnect window does not look like
    ///   the same slot from the primary's POV (which would evict
    ///   backlog frames the new embed still needs from offset 0).
    ///   Override via [`Config::with_replica_id`] when you specifically
    ///   want the slot to be re-claimed across restarts.
    ///
    /// For full builder control (custom replica id, backoff bounds,
    /// snapshot dir, etc.) use [`Self::open`] with
    /// [`Config::with_replica_upstream`] + the related setters
    /// instead.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_replica(upstream: impl Into<String>) -> io::Result<Self> {
        let cfg = Config::default()
            .without_aof()
            .with_replica_id(crate::replica_glue::fresh_replica_id())
            .with_replica_upstream(upstream);
        Self::open(cfg)
    }

    /// `true` when this store was opened against a replication
    /// upstream — local writes are rejected with `READONLY`.
    pub fn is_replica(&self) -> bool {
        self.config.replica_upstream.is_some()
    }

    /// Retarget this replica at a new primary URL (`host:port`). The
    /// runner picks up the change on its next connect — which is
    /// forced now by `shutdown`ing the current socket clone, so the
    /// retarget lands within `Config::replica_reconnect_min` (default
    /// 100 ms) of this call.
    ///
    /// Returns `Err` with `ErrorKind::InvalidInput` when this store is
    /// not a replica (no upstream was configured at open). Application
    /// code typically drives this from a kevy-elect failover signal —
    /// see `docs/cluster.md` "embed-as-read-replica" / Phase 2 / T2.7.
    /// kevy-embedded itself stays elect-protocol-agnostic; the
    /// integration glue lives in the application.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_replica_upstream(&self, new_upstream: impl Into<String>) -> io::Result<()> {
        if !self.is_replica() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "set_replica_upstream called on a non-replica store",
            ));
        }
        let Some(runner) = self.guard.replica_runner.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replica runner is not active (open was racy?)",
            ));
        };
        runner.set_upstream(new_upstream.into());
        Ok(())
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

    /// `KEYS` / `SCAN`-glob across **every shard** — the cross-shard
    /// replacement for `with(|s| s.collect_keys(pat, lim))`, which only sees
    /// shard 0 once sharding is on. Behaves identically to `with(...)` when
    /// `shard_count() == 1`. `limit` bounds the *total* returned across shards.
    /// Takes a read lock per shard (concurrent-safe).
    pub fn collect_keys(&self, pattern: Option<&[u8]>, limit: Option<usize>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for shard in self.shards.iter() {
            if limit.is_some_and(|l| out.len() >= l) {
                break;
            }
            let remaining = limit.map(|l| l - out.len());
            out.extend(lock_read(shard).store.collect_keys(pattern, remaining));
        }
        out
    }

    /// Run `f` against **each shard's** underlying `kevy_store::Store` (in
    /// shard-index order) — the cross-shard escape hatch. The caller assembles
    /// the merged result. Pairs with [`Self::shard_count`]. For a single key,
    /// prefer [`Self::with_key`]; for a glob scan, prefer [`Self::collect_keys`].
    pub fn for_each_shard<F: FnMut(&mut kevy_store::Store)>(&self, mut f: F) {
        for shard in self.shards.iter() {
            f(&mut lock_write(shard).store);
        }
    }

    /// Number of keyspace shards (`== Config::shards`).
    #[inline]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
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

    // Durability methods (`rewrite_aof`, `save_snapshot`) live in
    // `crate::store_persist` to keep this file under the 500-LOC
    // project ceiling.
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
    shard.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read-lock an `Inner`, recovering from poison.
pub(crate) fn lock_read(shard: &RwLock<Inner>) -> RwLockReadGuard<'_, Inner> {
    shard.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn log_argv(aof: &mut Option<Aof>, parts: &[&[u8]]) -> io::Result<()> {
    if let Some(aof) = aof {
        let argv = Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
        aof.append(&argv)?;
    }
    Ok(())
}

/// Complete a write on one shard: AOF-log the canonical RESP command,
/// publish to the embed-as-writer replication source (if configured),
/// then run that shard's post-write eviction sweep.
pub(crate) fn commit_write(inner: &mut Inner, parts: &[&[u8]]) -> io::Result<()> {
    log_argv(&mut inner.aof, parts)?;
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(src) = &inner.writer_source {
        crate::replica_source::push_into(src, parts);
    }
    inner.store.try_evict_after_write();
    Ok(())
}

pub(crate) fn store_err(e: StoreError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("kevy-store: {e:?}"))
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Stop the replica runner FIRST so no more frames arrive while
        // we're shutting down + flushing the AOF (frames would race
        // with the shutdown path).
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(r) = &self.replica_runner {
            r.shutdown();
        }
        // Stop the writer-source accept + connection threads next, so
        // no new replica picks up bytes mid-flush.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(rs) = &self.replica_source {
            rs.shutdown();
        }
        // Stop + join the reaper, then flush every shard's AOF so EverySec
        // users don't lose the last sub-second of writes.
        if let Some(stop) = &self.reaper_stop {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(j) = self
            .reaper_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
#[cfg(test)]
#[path = "store_tests_shard.rs"]
mod tests_shard;
#[cfg(test)]
#[path = "store_tests_p2.rs"]
mod tests_p2;
#[cfg(test)]
#[path = "store_tests_p3.rs"]
mod tests_p3;
#[cfg(test)]
#[path = "store_tests_bitmap.rs"]
mod tests_bitmap;
#[cfg(test)]
#[path = "store_tests_bonus.rs"]
mod tests_bonus;
#[cfg(test)]
#[path = "store_tests_scan.rs"]
mod tests_scan;
#[cfg(test)]
#[path = "store_tests_atomic.rs"]
mod tests_atomic;
#[cfg(test)]
#[path = "store_tests_more.rs"]
mod tests_more;
#[cfg(test)]
#[path = "store_tests_keyspace.rs"]
mod tests_keyspace;
