//! The [`Store`]'s shared internals — per-shard [`Inner`], the
//! last-clone [`DropGuard`], and the [`WeakStore`] handle (split out
//! of `store.rs` to keep it under the 500-LOC project ceiling;
//! behaviour unchanged).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread::JoinHandle;

#[cfg(feature = "persist")]
use kevy_persist::Aof;

use crate::config::Config;
use crate::pubsub::PubsubBus;
#[cfg(feature = "persist")]
use crate::store::lock_write;
use crate::store::{Shards, Store};

/// Weak handle to a `Store` — does not keep the underlying keyspace alive.
///
/// Used by the URL-keyed registry in `kevy-client` so that multiple
/// `Connection::connect("mem://name")` calls share the same backing store
/// without leaking it when all strong handles go away.
#[derive(Clone)]
pub struct WeakStore {
    shards: Weak<Vec<Arc<RwLock<Inner>>>>,
    guard: Weak<DropGuard>,
    config: Config,
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    feed_weak: Option<std::sync::Weak<Mutex<kevy_replicate::feed::FeedSource>>>,
    blocker_weak: Weak<crate::ops_blocking::Blocker>,
    #[cfg(feature = "index")]
    indexes_weak: Weak<crate::ops_index::IndexReg>,
    #[cfg(feature = "index")]
    views_weak: Weak<crate::ops_view::ViewReg>,
}

impl WeakStore {
    /// Try to upgrade back to a `Store`. Returns `None` if the last strong
    /// reference has already been dropped.
    pub fn upgrade(&self) -> Option<Store> {
        let guard = self.guard.upgrade()?;
        Some(Store {
            shards: self.shards.upgrade()?,
            config: self.config.clone(),
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            feed: self.feed_weak.as_ref().and_then(std::sync::Weak::upgrade),
            blocker: self.blocker_weak.upgrade()?,
            #[cfg(feature = "index")]
            indexes: self.indexes_weak.upgrade()?,
            #[cfg(feature = "index")]
            views: self.views_weak.upgrade()?,
            #[cfg(feature = "index")]
            tables: guard.tables.clone(),
            // The report rides the DropGuard (engine lifetime), so a
            // resurrection that outlives every full Store handle
            // still reports the ORIGINAL boot's replay verdict.
            open_report: guard.open_report.clone(),
            guard,
        })
    }
}

impl Store {
    /// Get a weak handle that does not keep the keyspace alive.
    pub fn downgrade(&self) -> WeakStore {
        WeakStore {
            shards: Arc::downgrade(&self.shards),
            guard: Arc::downgrade(&self.guard),
            config: self.config.clone(),
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            feed_weak: self.feed.as_ref().map(Arc::downgrade),
            blocker_weak: Arc::downgrade(&self.blocker),
            #[cfg(feature = "index")]
            indexes_weak: Arc::downgrade(&self.indexes),
            #[cfg(feature = "index")]
            views_weak: Arc::downgrade(&self.views),
        }
    }
}

pub(crate) struct Inner {
    pub(crate) store: kevy_store::Store,
    #[cfg(feature = "persist")]
    pub(crate) aof: Option<Aof>,
    /// Pub/sub bus. Only shard 0's is ever used (pub/sub is process-wide);
    /// other shards carry an idle one (cheap).
    pub(crate) bus: PubsubBus,
    /// Shared replication source if this store is an embed-as-writer.
    /// Every shard holds a clone of the same `Arc<Mutex<...>>` so
    /// `commit_write` can push mutations without reaching back up
    /// through the `DropGuard`.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub(crate) writer_source:
        Option<std::sync::Arc<Mutex<kevy_replicate::source::ReplicationSource>>>,
    /// CDC feed (one stream per store); every shard holds a clone
    /// so `commit_write` pushes effects inline. `None` = feed off.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub(crate) feed: Option<std::sync::Arc<Mutex<kevy_replicate::feed::FeedSource>>>,
    /// Blocking-pop wake channel clone (see `Store::blocker`).
    pub(crate) blocker: Option<Arc<crate::ops_blocking::Blocker>>,
    /// This shard's index segments + the store-level registry
    /// handle (for the commit_write hook).
    #[cfg(feature = "index")]
    pub(crate) idx_segs: crate::ops_index::ShardSegs,
    #[cfg(feature = "index")]
    pub(crate) idx_reg: Option<Arc<crate::ops_index::IndexReg>>,
    /// This shard's view states + registry handle.
    #[cfg(feature = "index")]
    pub(crate) view_segs: crate::ops_view::ShardViews,
    #[cfg(feature = "index")]
    pub(crate) view_reg: Option<Arc<crate::ops_view::ViewReg>>,
}

impl Inner {
    pub(crate) fn new(
        store: kevy_store::Store,
        #[cfg(feature = "persist")] aof: Option<Aof>,
    ) -> Self {
        Inner {
            store,
            #[cfg(feature = "persist")]
            aof,
            bus: PubsubBus::new(),
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            writer_source: None,
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            feed: None,
            blocker: None,
            #[cfg(feature = "index")]
            idx_segs: crate::ops_index::ShardSegs::default(),
            #[cfg(feature = "index")]
            idx_reg: None,
            #[cfg(feature = "index")]
            view_segs: crate::ops_view::ShardViews::default(),
            #[cfg(feature = "index")]
            view_reg: None,
        }
    }
}

/// Owns the reaper-thread handle + the shards for the final AOF flush. Lives
/// in an `Arc<DropGuard>` shared across every `Store` clone; the drop logic
/// fires only when the last clone goes away.
pub(crate) struct DropGuard {
    /// Set by [`Store::shutdown`]: every later write fails with
    /// `KevyError::Closed`. Shared across clones (it lives here so ANY
    /// clone's shutdown gates ALL clones' writes).
    pub(crate) shutdown: AtomicBool,
    /// The boot replay verdict. Owned by the guard (engine lifetime),
    /// so `WeakStore::upgrade` can rebuild a full `Store` — with the
    /// original boot's report — even after every full handle dropped
    /// while a subscription kept the engine alive.
    pub(crate) open_report: Arc<crate::metric::OpenReport>,
    /// The table registry — owned by the guard (engine lifetime) for
    /// the same reason as `open_report`: a `WeakStore::upgrade` after
    /// every full handle dropped must still see the declared tables.
    #[cfg(feature = "index")]
    pub(crate) tables: Arc<crate::ops_table::TableReg>,
    pub(crate) reaper_stop: Option<Arc<AtomicBool>>,
    pub(crate) reaper_join: Mutex<Option<JoinHandle<()>>>,
    // Read by the persist flush; without it the strong ref still
    // pins the shards until the LAST clone (incl. subscriptions) drops.
    #[cfg_attr(not(feature = "persist"), allow(dead_code))]
    pub(crate) shards_for_flush: Shards,
    /// Replica runner thread + reconnect machinery, present iff this
    /// store was opened with `Config::replica_upstream = Some(...)`.
    /// Joined here so the runner stops cleanly when the last `Store`
    /// clone goes away.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub(crate) replica_runner: Option<crate::replica_runner::ReplicaRunner>,
    /// Feed close-marker inputs — the feed handle + data dir,
    /// present iff feed enabled AND persistent. Written after the AOF
    /// flush so the marker's cursor describes durable state.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub(crate) feed_close:
        Option<(std::sync::Arc<Mutex<kevy_replicate::feed::FeedSource>>, std::path::PathBuf)>,
    /// Replica-source listener + accepted connection threads, present
    /// iff this store is an embed-as-writer
    /// (`Config::embed_writer_listen_addr = Some(...)`). Joined on
    /// last-clone drop.
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    pub(crate) replica_source: Option<crate::replica_source::ReplicaSource>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Stop the replica runner FIRST so no more frames arrive while
        // we're shutting down + flushing the AOF (frames would race
        // with the shutdown path).
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        if let Some(r) = &self.replica_runner {
            r.shutdown();
        }
        // Stop the writer-source accept + connection threads next, so
        // no new replica picks up bytes mid-flush.
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        if let Some(rs) = &self.replica_source {
            rs.shutdown();
        }
        // Stop + join the reaper, then flush every shard's AOF so EverySec
        // users don't lose the last sub-second of writes.
        if let Some(stop) = &self.reaper_stop {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(j) =
            self.reaper_join.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
        {
            let _ = j.join();
        }
        #[cfg(feature = "persist")]
        for shard in self.shards_for_flush.iter() {
            let mut g = lock_write(shard);
            if let Some(aof) = &mut g.aof {
                // Unconditional: `maybe_sync` is a no-op inside the EverySec
                // window, which let the fsynced close marker below claim
                // durability the AOF tail didn't have yet — a power loss in
                // that gap resumed the cursor over a rolled-back store, the
                // one phantom the generation fence cannot detect. Same
                // discipline as the server's shutdown_drain.
                let _ = aof.sync_now();
            }
        }
        // With the AOF durable, record the feed continuity
        // marker — the cursor now exactly describes on-disk state.
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        if let Some((feed, dir)) = &self.feed_close {
            Store::feed_write_close_marker(feed, dir);
        }
    }
}
