//! kevy-persist — durability for a [`kevy_store::Store`].
//!
//! Two mechanisms, both zero-dependency pure Rust over `std::fs`:
//!
//! - **Snapshot (RDB-style):** [`save_snapshot`] dumps a whole store to a temp
//!   file then atomically renames it (fsync before rename); [`load_snapshot`]
//!   restores it. A compact, type-tagged binary format.
//! - **AOF:** an [`Aof`] append-only command log with a configurable fsync
//!   policy; [`replay_aof`] re-applies it on startup, tolerating a truncated
//!   trailing frame from a crash mid-write.
//!
//! In a shared-nothing runtime each shard persists its own store to its own
//! file, so there is no cross-core coordination. Part of the [kevy] server.
//!
//! [kevy]: https://crates.io/crates/kevy
//!
//! # Example (AOF)
//!
//! ```
//! use kevy_persist::{Aof, Argv, Fsync, replay_aof};
//!
//! # fn main() -> std::io::Result<()> {
//! let path = std::env::temp_dir().join("kevy-persist-doctest.aof");
//! # let _ = std::fs::remove_file(&path);
//! {
//!     let mut aof = Aof::open(&path, Fsync::No)?;
//!     aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
//! } // flushed on drop
//!
//! let mut replayed: Vec<Argv> = Vec::new();
//! replay_aof(&path, |args| replayed.push(args))?;
//! assert_eq!(replayed, vec![vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]]);
//! # std::fs::remove_file(&path).ok();
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod aof;
#[cfg(not(target_arch = "wasm32"))]
mod aof_mapped;
mod aof_policy;
mod aof_queue;
mod aof_rewrite;
#[cfg(not(target_arch = "wasm32"))]
mod aof_stage;
#[cfg(target_arch = "wasm32")]
mod aof_stage_off;
mod aof_sync;
mod aof_txn;
mod aof_util;
mod baseline;
mod crc32c;
mod dir_lock;
mod dump_cache;
pub mod feed_meta;
pub mod layout;
mod log_base;
mod modes;
mod record;
mod record_pieces;
mod replay;
mod replay_log;
mod replay_report;
mod replay_resync;
mod replay_txn;
mod replay_walk;
pub mod reshard;
mod reshard_journal;
mod rewrite_chunk;
mod rewrite_fmt;
mod rewrite_frames;
mod rewrite_stream_fmt;
mod segmented;
mod shards_meta;
mod snapshot_aux;
mod snapshot_commit;
mod snapshot_fmt;
mod snapshot_payload;
mod snapshot_read;
mod snapshot_write;
#[cfg(not(target_arch = "wasm32"))]
mod stage_recover;
#[cfg(not(target_arch = "wasm32"))]
mod stage_ring;

pub use aof::{AOF_MAGIC, Aof};
pub use aof_policy::RewritePolicy;
pub use aof_rewrite::{RewritePlan, RewriteStats};
#[cfg(not(target_arch = "wasm32"))]
pub use aof_stage::StageOpen;
pub use aof_sync::PendingSync;
pub use baseline::estimate_rewrite_size;
pub use log_base::{is_log_base, settle_snapshot};
pub use modes::{Fsync, ReplayMode, ReplaySummary};
pub use record::{AOF2_MAGIC, AofFormat, RecordStep, next_record, write_record_multibulk};
pub use replay::{
    ReplayReport, replay_aof, replay_aof_in_place, replay_aof_quiet, replay_aof_resync,
};
pub use segmented::{SEGMENTED, segmented_argv, segmented_frame};

/// How often bulk-load paths check the tiering demote watermark:
/// every this many applied frames/records, the loading store runs
/// `demote_to_watermark`. Replay executes into the hot map, so a boot
/// whose dataset exceeds the tier budget would OOM before tiering ever
/// ran without the inline spill. One shared constant so AOF replay
/// (whose drive loops live in the callers — kevy-rt / kevy-embedded)
/// and the snapshot loader stride identically.
///
/// ```
/// // a replay loop demotes every `REPLAY_DEMOTE_INTERVAL` frames
/// let frames = 5000u64;
/// let demotions = (1..=frames)
///     .filter(|n| n.is_multiple_of(kevy_persist::REPLAY_DEMOTE_INTERVAL))
///     .count();
/// assert_eq!(demotions, 4);
/// ```
pub const REPLAY_DEMOTE_INTERVAL: u64 = 1024;
pub use dir_lock::DirLock;
pub use kevy_resp::{Argv, ArgvView};
use kevy_store::Store;
use kevy_store::Value;
pub(crate) use rewrite_fmt::estimate_multibulk_bytes;
pub use rewrite_fmt::{dump_aof, dump_store_to_buf, write_multibulk};
pub use rewrite_frames::value_as_v1_frames;
pub use rewrite_stream_fmt::write_stream_as_commands;
pub use shards_meta::{Routing, ShardsMeta};
pub use snapshot_aux::WithAux;
pub(crate) use snapshot_fmt::{SNAPSHOT_BUF_CAP, write_bytes};
pub use snapshot_read::{
    load_snapshot, load_snapshot_filtered, load_snapshot_from, load_snapshot_with_aux,
    read_snapshot_cursor,
};
pub(crate) use snapshot_write::write_stream_groups;
pub use snapshot_write::{
    save_snapshot, write_snapshot_tmp, write_snapshot_tmp_with_cursor, write_snapshot_to,
    write_snapshot_to_with_cursor,
};

/// Anything that can enumerate `(key, &Value, ttl_ms)` triples for
/// serialization: a live [`Store`] (its `snapshot_each`, the synchronous
/// paths) or a frozen [`kevy_store::SnapshotView`] (the COW paths — collect
/// on the owning thread, serialize on a background one).
///
/// **Tiering contract**: `for_each_entry` yields VLOG-backed
/// `Value::Cold` stubs materialized (the store reads its own log; a
/// view reads through the `Arc<VlogFile>` pins captured at collect
/// time) one value at a time, so serializer memory stays bounded and
/// nothing is ever promoted into the hot map. SEG-backed stubs pass
/// through AS STUBS: their data is truth in the segment directory, and
/// the consumers persist the reference, not the payload.
///
/// Hosts implement it for their own aggregates (the embedded store
/// serializes several shards as one source). An implementation must uphold
/// the tiering contract above, and yield each live key exactly once.
///
/// ```
/// use kevy_persist::SnapshotSource;
/// use kevy_store::{SetCondition, Store, Value};
///
/// // two shards snapshotted as one source
/// struct Both(Store, Store);
/// impl SnapshotSource for Both {
///     fn for_each_entry(&self, mut f: impl FnMut(&[u8], &Value, Option<u64>)) {
///         self.0.for_each_entry(&mut f);
///         self.1.for_each_entry(&mut f);
///     }
/// }
///
/// let (mut a, mut b) = (Store::new(), Store::new());
/// a.set(b"a", b"1".to_vec(), None, SetCondition::Always);
/// b.set(b"b", b"2".to_vec(), None, SetCondition::Always);
/// let mut image = Vec::new();
/// kevy_persist::write_snapshot_to(&Both(a, b), &mut image)?;
///
/// let mut back = Store::new();
/// kevy_persist::load_snapshot_from(&mut back, image.as_slice())?;
/// assert_eq!(back.dbsize(), 2);
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait SnapshotSource {
    /// Visit every live entry as `(key, &value, remaining_ttl_ms)`.
    ///
    /// ```
    /// use kevy_persist::SnapshotSource;
    /// use kevy_store::{SetCondition, Store};
    /// use std::time::Duration;
    ///
    /// let mut store = Store::new();
    /// store.set(b"k", b"v".to_vec(), Some(Duration::from_secs(60)), SetCondition::Always);
    /// let mut seen = Vec::new();
    /// store.for_each_entry(|key, _value, ttl| seen.push((key.to_vec(), ttl.is_some())));
    /// assert_eq!(seen, [(b"k".to_vec(), true)]);
    /// ```
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>));

    /// Visit every live hash field TTL as `(key, field,
    /// absolute_unix_ms)`. Default = none (sources without the
    /// feature).
    ///
    /// ```
    /// use kevy_persist::SnapshotSource;
    /// use kevy_store::{HExpireCond, Store};
    ///
    /// let mut store = Store::new();
    /// store.hset(b"h", &[(b"f", b"v")])?;
    /// let deadline = kevy_store::now_unix_ms() + 60_000;
    /// store.hexpire_at(b"h", &[b"f"], deadline, HExpireCond::Always)?;
    /// let mut ttls = Vec::new();
    /// store.for_each_hash_ttl(|key, field, at| ttls.push((key.to_vec(), field.to_vec(), at)));
    /// assert_eq!(ttls, [(b"h".to_vec(), b"f".to_vec(), deadline)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    fn for_each_hash_ttl(&self, _f: impl FnMut(&[u8], &[u8], u64)) {}

    /// The live row segments' `(seq, file)` identities — the AOF
    /// rewrite's trailing SEGMENTED frames and the snapshot writer's
    /// version choice read these. Default = none.
    ///
    /// ```
    /// use kevy_persist::SnapshotSource;
    ///
    /// // a store that never sealed a row segment references none
    /// assert!(SnapshotSource::row_seg_files(&kevy_store::Store::new()).is_empty());
    /// ```
    fn row_seg_files(&self) -> Vec<(u32, String)> {
        Vec::new()
    }

    /// A record frame the runtime keeps beside the keyspace (the server's
    /// index catalog is one). A snapshot stores it as its last record and a
    /// rewritten log as its last frame, so it survives both. Default =
    /// none.
    ///
    /// ```
    /// use kevy_persist::SnapshotSource;
    ///
    /// assert!(SnapshotSource::aux_frame(&kevy_store::Store::new()).is_none());
    /// ```
    fn aux_frame(&self) -> Option<Argv> {
        None
    }
}

/// Whether `v` is a row-segment stub (persisted as a reference).
pub(crate) fn is_seg_stub(v: &Value) -> bool {
    matches!(v, Value::Cold(c) if c.seg_parts().is_some())
}

impl SnapshotSource for Store {
    fn for_each_entry(&self, mut f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.snapshot_each(|k, v, ttl| {
            if is_seg_stub(v) {
                return f(k, v, ttl);
            }
            match self.materialize_cold(k, v) {
                // Vlog stub: decode the record into a transient hot
                // value (dropped after the callback — memory bound =
                // one value) and emit exactly what the hot value
                // would have.
                Some(hot) => f(k, &hot, ttl),
                None => f(k, v, ttl),
            }
        });
    }
    fn for_each_hash_ttl(&self, f: impl FnMut(&[u8], &[u8], u64)) {
        self.hash_ttl_each(f);
    }
    fn row_seg_files(&self) -> Vec<(u32, String)> {
        self.row_seg_files()
    }
}

impl SnapshotSource for kevy_store::SnapshotView {
    fn for_each_entry(&self, mut f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.each(|k, v, ttl| {
            if is_seg_stub(v) {
                return f(k, v, ttl);
            }
            match self.materialize_cold(k, v) {
                // Vlog stub: resolve against the view's pinned files —
                // the serializer thread never touches the store.
                Some(hot) => f(k, &hot, ttl),
                None => f(k, v, ttl),
            }
        });
    }
    fn row_seg_files(&self) -> Vec<(u32, String)> {
        kevy_store::SnapshotView::row_seg_files(self)
    }
    fn for_each_hash_ttl(&self, f: impl FnMut(&[u8], &[u8], u64)) {
        self.each_hash_ttl(f);
    }
}

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Aof>();
    send_sync::<Fsync>();
    send_sync::<ReplayMode>();
    send_sync::<ReplaySummary>();
    send_sync::<RewritePlan>();
    send_sync::<RewriteStats>();
    send_sync::<RewritePolicy>();
    #[cfg(not(target_arch = "wasm32"))]
    send_sync::<StageOpen>();
    send_sync::<PendingSync>();
    send_sync::<ReplayReport>();
    send_sync::<AofFormat>();
    send_sync::<RecordStep<'static>>();
    send_sync::<Routing>();
    send_sync::<ShardsMeta>();

    send_sync::<reshard::StdLayout>();
    send_sync::<DirLock>();
};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_aof;
#[cfg(all(test, unix, not(target_arch = "wasm32")))]
mod tests_fail;
#[cfg(test)]
mod tests_log_base;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests_log_base_fail;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests_mapped;
#[cfg(test)]
mod tests_policy;
#[cfg(test)]
mod tests_rewrite;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests_stage;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests_stage_aof;
#[cfg(test)]
mod tests_sync;
#[cfg(test)]
mod tests_tier_stream;
#[cfg(test)]
mod tests_txn_tail;
#[cfg(all(test, unix))]
mod tests_zero_tail;
