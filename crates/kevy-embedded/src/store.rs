//! [`Store`] — the embedded entry point. Wraps `kevy_store::Store` with
//! per-shard locks (for cross-thread access), optional AOF auto-logging, an
//! optional background TTL reaper, and an in-process pub/sub bus.

use crate::KevyError;
use crate::KevyResult;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(feature = "persist")]
use kevy_persist::Argv;
use kevy_store::ExpireStats;

use crate::config::Config;
use crate::shard::{build_shards, shard_idx};

pub use crate::store_inner::WeakStore;
pub(crate) use crate::store_inner::{DropGuard, Inner};

/// The write gate every mutating facade entry crosses: rejects writes after
/// [`Store::shutdown`] with [`KevyError::Closed`], and every local write on
/// a replica with `READONLY`. One atomic load — free on the hot path.
pub(crate) fn ensure_writable(store: &Store) -> Result<(), KevyError> {
    if store.guard.shutdown.load(std::sync::atomic::Ordering::Acquire) {
        return Err(KevyError::Closed);
    }
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    if store.is_replica() {
        return Err(KevyError::ReadOnly);
    }
    Ok(())
}

/// The keyspace shards (`hash(key) % n`), each a fully independent
/// `kevy_store::Store` + AOF behind its own lock. `n == 1` (the default) is a
/// one-element vec = the original single-lock store.
pub(crate) type Shards = Arc<Vec<Arc<RwLock<Inner>>>>;

/// The embedded keyspace.
///
/// **`Store` is `Clone`**. A clone is a cheap `Arc` bump:
/// every clone reaches the same underlying shards + AOF + reaper + pub/sub
/// bus. The reaper thread is joined and each shard's AOF is flushed exactly
/// once, when the **last** clone is dropped.
///
/// ```
/// use kevy_embedded::{Config, Store};
///
/// # fn main() -> kevy_embedded::KevyResult<()> {
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
    /// CDC feed handle (read API side); shards carry clones for
    /// the write side. `None` = feed off (or wasm).
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub(crate) feed: Option<std::sync::Arc<Mutex<kevy_replicate::feed::FeedSource>>>,
    /// Blocking-pop wake channel (always present; writers pay one
    /// Relaxed load while nobody blocks).
    pub(crate) blocker: Arc<crate::ops_blocking::Blocker>,
    /// Index registry (catalog + version).
    #[cfg(feature = "index")]
    pub(crate) indexes: Arc<crate::ops_index::IndexReg>,
    /// View registry.
    #[cfg(feature = "index")]
    pub(crate) views: Arc<crate::ops_view::ViewReg>,
    /// What this open's replay restored — and what it could not.
    pub(crate) open_report: Arc<crate::metric::OpenReport>,
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
    pub fn open(config: Config) -> KevyResult<Self> {
        Self::open_inner(config)
    }

    /// What this open's replay restored — and, crucially, what it could
    /// NOT: `dropped_bytes > 0` or `corrupt` means the store recovered
    /// less than the files held (the dropped region was quarantined). Turn
    /// this into a startup health check / alert — the machine-readable
    /// twin of the boot WARN line.
    pub fn open_report(&self) -> &crate::metric::OpenReport {
        &self.open_report
    }

    /// Answer one RESP request against this store using the SAME
    /// read-only verb whitelist the embedded RESP listener serves
    /// (`Config::with_resp_listener`). The reply is appended to `out`
    /// as raw RESP bytes; write verbs answer `-ERR` like the listener
    /// does. This is the programmatic face of the listener — tooling
    /// (e.g. `kevy-cli --embed`) inspects a store without a socket.
    #[cfg(all(feature = "listener", not(target_arch = "wasm32")))]
    pub fn dispatch_readonly(&self, argv: &[Vec<u8>], out: &mut Vec<u8>) {
        crate::listener::verbs_dispatch(self, argv, out);
    }

    fn open_inner(config: Config) -> KevyResult<Self> {
        let (shards, open_report) = build_shards(&config)?;
        let shards: Shards = Arc::new(shards);
        let (reaper_stop, reaper_join) = crate::reaper::spawn_reaper(&config, &shards)?;
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        let (replica_runner, replica_source, feed) =
            crate::store_wire::wire_replication(&config, &shards)?;
        let blocker = crate::store_wire::wire_blocker(&shards);
        #[cfg(feature = "index")]
        let (indexes, views) = crate::store_wire::wire_registries(&shards);
        let open_report = Arc::new(open_report);
        let guard = Arc::new(DropGuard {
            shutdown: std::sync::atomic::AtomicBool::new(false),
            // Owned here (engine lifetime), not only by Store handles:
            // WeakStore::upgrade rebuilds a Store from the guard, and
            // a registry resurrection (kevy-client `mem://`) can
            // outlive every full Store handle. Requiring a live
            // Store-held Arc broke exactly that (publish saw a
            // second, empty bus).
            open_report: open_report.clone(),
            reaper_stop,
            reaper_join: Mutex::new(reaper_join),
            shards_for_flush: shards.clone(),
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            replica_runner,
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            feed_close: match (&feed, &config.data_dir) {
                (Some(f), Some(d)) => Some((f.clone(), d.clone())),
                _ => None,
            },
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            replica_source,
        });
        let store = Store {
            shards,
            guard,
            config,
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            feed,
            blocker,
            #[cfg(feature = "index")]
            indexes,
            #[cfg(feature = "index")]
            views,
            open_report,
        };
        store.boot_ancillary()?;
        Ok(store)
    }

    /// Post-construction bring-up: index/view boot scans and the
    /// optional read-only RESP listener. Split from [`Self::open_inner`]
    /// for the fn-length rule.
    fn boot_ancillary(&self) -> KevyResult<()> {
        #[cfg(feature = "index")]
        self.idx_boot();
        #[cfg(feature = "index")]
        self.view_boot();
        #[cfg(all(feature = "listener", not(target_arch = "wasm32")))]
        if let Some(addr) = self.config.resp_listener {
            crate::listener::spawn(addr, self.downgrade())?;
        }
        Ok(())
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
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub fn open_replica(upstream: impl Into<String>) -> KevyResult<Self> {
        let cfg = Config::default()
            .without_aof()
            .with_replica_id(crate::replica_glue::fresh_replica_id())
            .with_replica_upstream(upstream);
        Self::open(cfg)
    }

    /// `true` when this store was opened against a replication
    /// upstream — local writes are rejected with `READONLY`.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub fn is_replica(&self) -> bool {
        self.config.replica_upstream.is_some()
    }

    /// Flush every shard's AOF to disk (a real fsync), write the feed
    /// continuity marker, then refuse every later write (they fail with
    /// [`KevyError::Closed`]; reads stay available). Idempotent and
    /// clone-safe: any clone's `shutdown` gates them all, so a signal
    /// handler's teardown is two deterministic lines —
    /// `store.shutdown()?; std::process::exit(0)` — instead of praying
    /// every task's `Arc<Store>` drops in time. Writes racing the call
    /// may land after the fsync; writes issued after it returns cannot.
    pub fn shutdown(&self) -> std::io::Result<()> {
        use std::sync::atomic::Ordering;
        self.guard.shutdown.store(true, Ordering::Release);
        #[cfg(feature = "persist")]
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            if let Some(aof) = &mut g.aof {
                aof.sync_now()?;
            }
        }
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        if let (Some(feed), Some(dir)) = (&self.feed, &self.config.data_dir) {
            Store::feed_write_close_marker(feed, dir);
        }
        Ok(())
    }

    /// Retarget this replica at a new primary URL (`host:port`). The
    /// runner picks up the change on its next connect — which is
    /// forced now by `shutdown`ing the current socket clone, so the
    /// retarget lands within `Config::replica_reconnect_min` (default
    /// 100 ms) of this call.
    ///
    /// Returns `Err` with `ErrorKind::InvalidInput` when this store is
    /// not a replica (no upstream was configured at open). Application
    /// code typically drives this from a `kevy-elect` failover signal —
    /// see [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md).
    /// `kevy-embedded` itself stays elect-protocol-agnostic; the
    /// integration glue lives in the application.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub fn set_replica_upstream(&self, new_upstream: impl Into<String>) -> KevyResult<()> {
        if !self.is_replica() {
            return Err(KevyError::InvalidInput("set_replica_upstream called on a non-replica store".into()));
        }
        let Some(runner) = self.guard.replica_runner.as_ref() else {
            return Err(KevyError::InvalidInput("replica runner is not active (open was racy?)".into()));
        };
        runner.set_upstream(new_upstream.into());
        Ok(())
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
    #[cfg(feature = "persist")]
    pub fn log(&self, parts: &[&[u8]]) -> KevyResult<()> {
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
                // Tiering: tick continuation of the budgeted spill.
                let _ = g.store.demote_step();
                g.store.tick_expire(self.config.reaper_samples, self.config.reaper_max_rounds)
            };
            total.sampled += stats.sampled;
            total.expired += stats.expired;
            // Auto-rewrite rides the caller-driven tick in Manual mode; the
            // non-blocking path releases the lock for the disk spill.
            #[cfg(feature = "persist")]
            crate::reaper::concurrent_auto_rewrite(
                shard,
                kevy_persist::RewritePolicy {
                    pct: self.config.auto_aof_rewrite_pct,
                    min_size: self.config.auto_aof_rewrite_min_size,
                    bytes: self.config.auto_aof_rewrite_bytes,
                    interval_secs: self.config.auto_aof_rewrite_interval_secs,
                },
                self.config.metric_sink.as_ref(),
            );
        }
        total
    }

    /// The B9 transparency suite's deterministic demotion seam
    /// (`KEVY_TEST_FORCE_DEMOTE` genre): demote `key` to the cold tier
    /// NOW, ignoring the watermark — the suite drives cold state
    /// per-key, never by eviction timing. Returns whether a demotion
    /// happened (false: tiering off / key absent / not spillable).
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    #[doc(hidden)]
    pub fn debug_force_demote(&self, key: &[u8]) -> bool {
        self.wshard(key).store.debug_force_demote(key)
    }

    /// Tiering counters summed across shards:
    /// `(demotions_total, promotions_total)` — the B12 surface (INFO
    /// gauges land in T5). Zeros when tiering is off.
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub fn tier_counters(&self) -> (u64, u64) {
        let mut d = 0u64;
        let mut p = 0u64;
        for shard in self.shards.iter() {
            let s = lock_read(shard).store.tier_stats();
            d += s.demotions_total;
            p += s.promotions_total;
        }
        (d, p)
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

    /// Read-lock variant of [`Self::sum_shards`]: takes each shard's SHARED
    /// lock for read-only aggregations (DBSIZE etc.) that never mutate the
    /// keyspace — the underlying counter methods are all `&self`.
    pub(crate) fn sum_shards_read<F: Fn(&Inner) -> usize>(&self, f: F) -> usize {
        self.shards.iter().map(|s| f(&lock_read(s))).sum()
    }

    /// `u64` read-lock variant of [`Self::sum_shards_read`].
    pub(crate) fn sum_shards_u64_read<F: Fn(&Inner) -> u64>(&self, f: F) -> u64 {
        self.shards.iter().map(|s| f(&lock_read(s))).sum()
    }

    /// Run a fallible `f` over every shard (mutating, e.g. FLUSHALL).
    pub(crate) fn try_for_each_shard<F: FnMut(&mut Inner) -> KevyResult<()>>(
        &self,
        mut f: F,
    ) -> KevyResult<()> {
        for s in self.shards.iter() {
            f(&mut lock_write(s))?;
        }
        Ok(())
    }
}


pub(crate) use crate::store_glue::{commit_write, lock_read, lock_write, store_err};

#[cfg(test)]
#[path = "store_test_suites.rs"]
mod test_suites;
