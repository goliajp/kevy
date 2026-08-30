//! Open-time wiring helpers split from `store.rs` to honour the 500-LOC
//! cap: replication bring-up (embed-as-replica runner + embed-as-writer
//! source + CDC feed), the blocking-pop waker, and the index/view
//! registries. Each threads a shared handle into every shard's `Inner`
//! once, at open, under that shard's write lock (reads of the handle
//! afterwards are uncontended).

use std::sync::Arc;

use crate::KevyResult;
use crate::store::Shards;
use crate::store_glue::lock_write;

/// The engine backbone [`crate::store::Store::open`] stands on: the
/// shards, the boot report, the table registry, and the reaper.
/// Split from `open_inner` for the fn-length rule.
pub(crate) struct Backbone {
    pub(crate) shards: Shards,
    pub(crate) open_report: crate::metric::OpenReport,
    #[cfg(feature = "index")]
    pub(crate) tables: Arc<crate::ops_table::TableReg>,
    pub(crate) reaper_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) reaper_join: Option<std::thread::JoinHandle<()>>,
}

pub(crate) fn boot_backbone(config: &crate::config::Config) -> KevyResult<Backbone> {
    let (shards, open_report) = crate::shard::build_shards(config)?;
    let shards: Shards = Arc::new(shards);
    #[cfg(feature = "index")]
    let tables = Arc::new(crate::ops_table::TableReg::default());
    let (reaper_stop, reaper_join) = crate::reaper::spawn_reaper(
        config,
        &shards,
        #[cfg(feature = "index")]
        &tables,
    )?;
    Ok(Backbone {
        shards,
        open_report,
        #[cfg(feature = "index")]
        tables,
        reaper_stop,
        reaper_join,
    })
}

/// Replication bring-up half of `open_inner`: the replica runner
/// (embed-as-replica), the writer source (embed-as-writer) and the CDC
/// feed, with the feed handle cloned into every shard for the write path.
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
#[allow(clippy::type_complexity)] // inline tuple keeps the bring-up outputs colocated
pub(crate) fn wire_replication(
    config: &crate::config::Config,
    shards: &Shards,
) -> crate::KevyResult<(
    Option<crate::replica_runner::ReplicaRunner>,
    Option<crate::replica_source::ReplicaSource>,
    Option<Arc<std::sync::Mutex<kevy_replicate::feed::FeedSource>>>,
)> {
    let replica_runner = crate::replica_glue::spawn_replica_runner(config, shards);
    let replica_source = spawn_writer_source(config, shards)?;
    let feed = crate::store::Store::feed_open(config)?;
    if let Some(f) = &feed {
        for shard in shards.iter() {
            let mut g = lock_write(shard);
            g.feed = Some(f.clone());
        }
    }
    Ok((replica_runner, replica_source, feed))
}

/// Spawn the embed-as-writer replication source when
/// `Config::embed_writer_listen_addr` is set, wiring the shared source
/// into every shard's `Inner` so `commit_write` pushes mutations into
/// the backlog inline (done once at open under the shard's write lock;
/// reads of `Inner::writer_source` afterwards are uncontended).
///
/// The snapshot provider freezes every shard's COW view under the
/// source lock (so ack_offset and the frozen keyspace are one point in
/// time — writes between the two would replay twice otherwise), then
/// serializes outside the locks via the persist writer.
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
fn spawn_writer_source(
    config: &crate::config::Config,
    shards: &Shards,
) -> crate::KevyResult<Option<crate::replica_source::ReplicaSource>> {
    let Some(addr) = config.embed_writer_listen_addr.as_ref() else {
        return Ok(None);
    };
    let shards_for_snap: Shards = Arc::clone(shards);
    let snapshot: crate::replica_source::SnapshotProvider =
        Arc::new(move || crate::replica_source::freeze_and_serialize(&shards_for_snap));
    let rs = crate::replica_source::ReplicaSource::spawn(
        addr,
        config.embed_writer_backlog_bytes,
        snapshot,
    )?;
    let shared = rs.shared_source();
    for shard in shards.iter() {
        let mut g = lock_write(shard);
        g.writer_source = Some(shared.clone());
    }
    Ok(Some(rs))
}

/// Create the store-level blocking-pop waker and hand every shard's
/// `Inner` a clone.
pub(crate) fn wire_blocker(shards: &Shards) -> Arc<crate::ops_blocking::Blocker> {
    let blocker = Arc::new(crate::ops_blocking::Blocker::new());
    for shard in shards.iter() {
        lock_write(shard).blocker = Some(blocker.clone());
    }
    blocker
}

/// Create the store-level index + view catalogs and hand every shard's
/// `Inner` a clone.
#[cfg(feature = "index")]
pub(crate) fn wire_registries(
    shards: &Shards,
) -> (Arc<crate::ops_index::IndexReg>, Arc<crate::ops_view::ViewReg>) {
    let indexes = Arc::new(crate::ops_index::IndexReg::default());
    let views = Arc::new(crate::ops_view::ViewReg::default());
    for shard in shards.iter() {
        let mut g = lock_write(shard);
        g.idx_reg = Some(indexes.clone());
        g.view_reg = Some(views.clone());
    }
    (indexes, views)
}

/// The engine-lifetime `DropGuard` (owner of the boot report and the
/// table registry — `WeakStore::upgrade` rebuilds a full `Store` from
/// it even after every strong handle dropped while a subscription kept
/// the engine alive). Split from `open_inner` for the fn-length rule.
#[allow(clippy::too_many_arguments)] // one-call-site bring-up bundle
pub(crate) fn build_guard(
    open_report: &Arc<crate::metric::OpenReport>,
    reaper_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    reaper_join: Option<std::thread::JoinHandle<()>>,
    shards: &Shards,
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))] replica_runner: Option<
        crate::replica_runner::ReplicaRunner,
    >,
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))] replica_source: Option<
        crate::replica_source::ReplicaSource,
    >,
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))] feed: &Option<
        Arc<std::sync::Mutex<kevy_replicate::feed::FeedSource>>,
    >,
    config: &crate::config::Config,
    #[cfg(feature = "index")] tables: &Arc<crate::ops_table::TableReg>,
) -> Arc<crate::store_inner::DropGuard> {
    #[cfg(any(target_arch = "wasm32", not(feature = "replicate")))]
    let _ = config;
    Arc::new(crate::store_inner::DropGuard {
        shutdown: std::sync::atomic::AtomicBool::new(false),
        open_report: open_report.clone(),
        #[cfg(feature = "index")]
        tables: tables.clone(),
        reaper_stop,
        reaper_join: std::sync::Mutex::new(reaper_join),
        shards_for_flush: shards.clone(),
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        replica_runner,
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        feed_close: match (feed, &config.data_dir) {
            (Some(f), Some(d)) => Some((f.clone(), d.clone())),
            _ => None,
        },
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        replica_source,
    })
}
