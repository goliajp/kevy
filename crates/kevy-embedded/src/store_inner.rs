//! The [`Store`]'s shared internals — per-shard [`Inner`], the
//! last-clone [`DropGuard`], and the [`WeakStore`] handle (split out
//! of `store.rs` to keep it under the 500-LOC project ceiling;
//! behaviour unchanged).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread::JoinHandle;

use kevy_persist::Aof;

use crate::config::Config;
use crate::pubsub::PubsubBus;
use crate::store::{Shards, Store, lock_write};

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
    #[cfg(not(target_arch = "wasm32"))]
    feed_weak: Option<std::sync::Weak<Mutex<kevy_replicate::feed::FeedSource>>>,
    blocker_weak: Weak<crate::ops_blocking::Blocker>,
    indexes_weak: Weak<crate::ops_index::IndexReg>,
    views_weak: Weak<crate::ops_view::ViewReg>,
}

impl WeakStore {
    /// Try to upgrade back to a `Store`. Returns `None` if the last strong
    /// reference has already been dropped.
    pub fn upgrade(&self) -> Option<Store> {
        Some(Store {
            shards: self.shards.upgrade()?,
            guard: self.guard.upgrade()?,
            config: self.config.clone(),
            #[cfg(not(target_arch = "wasm32"))]
            feed: self.feed_weak.as_ref().and_then(std::sync::Weak::upgrade),
            blocker: self.blocker_weak.upgrade()?,
            indexes: self.indexes_weak.upgrade()?,
            views: self.views_weak.upgrade()?,
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
            #[cfg(not(target_arch = "wasm32"))]
            feed_weak: self.feed.as_ref().map(Arc::downgrade),
            blocker_weak: Arc::downgrade(&self.blocker),
            indexes_weak: Arc::downgrade(&self.indexes),
            views_weak: Arc::downgrade(&self.views),
        }
    }
}

pub(crate) struct Inner {
    pub(crate) store: kevy_store::Store,
    pub(crate) aof: Option<Aof>,
    /// Pub/sub bus. Only shard 0's is ever used (pub/sub is process-wide);
    /// other shards carry an idle one (cheap).
    pub(crate) bus: PubsubBus,
    /// Shared replication source if this store is an embed-as-writer.
    /// Every shard holds a clone of the same `Arc<Mutex<...>>` so
    /// `commit_write` can push mutations without reaching back up
    /// through the `DropGuard`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) writer_source:
        Option<std::sync::Arc<Mutex<kevy_replicate::source::ReplicationSource>>>,
    /// CDC feed (one stream per store); every shard holds a clone
    /// so `commit_write` pushes effects inline. `None` = feed off.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) feed: Option<std::sync::Arc<Mutex<kevy_replicate::feed::FeedSource>>>,
    /// Blocking-pop wake channel clone (see `Store::blocker`).
    pub(crate) blocker: Option<Arc<crate::ops_blocking::Blocker>>,
    /// This shard's index segments + the store-level registry
    /// handle (for the commit_write hook).
    pub(crate) idx_segs: crate::ops_index::ShardSegs,
    pub(crate) idx_reg: Option<Arc<crate::ops_index::IndexReg>>,
    /// This shard's view states + registry handle.
    pub(crate) view_segs: crate::ops_view::ShardViews,
    pub(crate) view_reg: Option<Arc<crate::ops_view::ViewReg>>,
}

impl Inner {
    pub(crate) fn new(store: kevy_store::Store, aof: Option<Aof>) -> Self {
        Inner {
            store,
            aof,
            bus: PubsubBus::new(),
            #[cfg(not(target_arch = "wasm32"))]
            writer_source: None,
            #[cfg(not(target_arch = "wasm32"))]
            feed: None,
            blocker: None,
            idx_segs: crate::ops_index::ShardSegs::default(),
            idx_reg: None,
            view_segs: crate::ops_view::ShardViews::default(),
            view_reg: None,
        }
    }
}

/// Owns the reaper-thread handle + the shards for the final AOF flush. Lives
/// in an `Arc<DropGuard>` shared across every `Store` clone; the drop logic
/// fires only when the last clone goes away.
pub(crate) struct DropGuard {
    pub(crate) reaper_stop: Option<Arc<AtomicBool>>,
    pub(crate) reaper_join: Mutex<Option<JoinHandle<()>>>,
    pub(crate) shards_for_flush: Shards,
    /// Replica runner thread + reconnect machinery, present iff this
    /// store was opened with `Config::replica_upstream = Some(...)`.
    /// Joined here so the runner stops cleanly when the last `Store`
    /// clone goes away.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) replica_runner: Option<crate::replica_runner::ReplicaRunner>,
    /// Feed close-marker inputs — the feed handle + data dir,
    /// present iff feed enabled AND persistent. Written after the AOF
    /// flush so the marker's cursor describes durable state.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) feed_close: Option<(
        std::sync::Arc<Mutex<kevy_replicate::feed::FeedSource>>,
        std::path::PathBuf,
    )>,
    /// Replica-source listener + accepted connection threads, present
    /// iff this store is an embed-as-writer
    /// (`Config::embed_writer_listen_addr = Some(...)`). Joined on
    /// last-clone drop.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) replica_source: Option<crate::replica_source::ReplicaSource>,
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
        // With the AOF durable, record the feed continuity
        // marker — the cursor now exactly describes on-disk state.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((feed, dir)) = &self.feed_close {
            Store::feed_write_close_marker(feed, dir);
        }
    }
}
