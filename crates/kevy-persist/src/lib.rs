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
mod aof_txn;
pub mod feed_meta;
pub mod layout;
mod replay;
mod replay_txn;
mod segmented;
mod aof_policy;
mod aof_util;
mod crc32c;
mod record;
mod replay_resync;
pub mod reshard;
mod rewrite_chunk;
mod rewrite_fmt;
mod rewrite_frames;
mod shards_meta;
mod snapshot_fmt;
mod snapshot_payload;
mod snapshot_read;
mod snapshot_write;

pub use aof::{AOF_MAGIC, Aof, Fsync, RewritePlan, RewriteStats};
pub use aof_util::write_aof_base;
pub use aof_policy::RewritePolicy;
pub use record::{AOF2_MAGIC, AofFormat, RecordStep, next_record, write_record_multibulk};
pub use replay::{ReplayReport, replay_aof, replay_aof_resync};
pub use segmented::{SEGMENTED, segmented_argv, segmented_frame};

/// How often bulk-load paths check the tiering demote watermark:
/// every this many applied frames/records, the loading store runs
/// `demote_to_watermark`. Replay executes into the hot map, so a boot
/// whose dataset exceeds the tier budget would OOM before tiering ever
/// ran without the inline spill. One shared constant so AOF replay
/// (whose drive loops live in the callers — kevy-rt / kevy-embedded)
/// and the snapshot loader stride identically.
pub const REPLAY_DEMOTE_INTERVAL: u64 = 1024;
pub use shards_meta::{Routing, ShardsMeta, read_shards_meta, write_shards_meta};
pub use kevy_resp::{Argv, ArgvView};
pub use rewrite_fmt::{dump_aof, dump_store_to_buf, write_multibulk, write_stream_as_commands};
pub use rewrite_frames::value_as_v1_frames;
pub use snapshot_read::{
    load_snapshot, load_snapshot_filtered, load_snapshot_from, read_snapshot_cursor,
};
pub use snapshot_write::{
    save_snapshot, write_snapshot_tmp, write_snapshot_to, write_snapshot_to_with_cursor,
};
pub(crate) use rewrite_fmt::estimate_multibulk_bytes;
pub(crate) use snapshot_fmt::{SNAPSHOT_BUF_CAP, write_bytes};
pub(crate) use snapshot_write::write_stream_groups;
use kevy_store::Store;
use kevy_store::Value;

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
pub trait SnapshotSource {
    /// Visit every live entry as `(key, &value, remaining_ttl_ms)`.
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>));

    /// Visit every live hash field TTL as `(key, field,
    /// absolute_unix_ms)`. Default = none (sources without the
    /// feature).
    fn for_each_hash_ttl(&self, _f: impl FnMut(&[u8], &[u8], u64)) {}

    /// The live row segments' `(seq, file)` identities — the AOF
    /// rewrite's trailing SEGMENTED frames and the snapshot writer's
    /// version choice read these. Default = none.
    fn row_seg_files(&self) -> Vec<(u32, String)> {
        Vec::new()
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

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_aof;
#[cfg(test)]
mod tests_rewrite;
#[cfg(test)]
mod tests_tier_stream;
