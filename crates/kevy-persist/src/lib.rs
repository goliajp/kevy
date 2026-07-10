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
pub mod feed_meta;
pub mod layout;
mod replay;
pub mod reshard;
mod rewrite_fmt;
mod shards_meta;
mod snapshot_fmt;
mod snapshot_payload;
mod snapshot_read;
mod snapshot_write;

pub use aof::{Aof, Fsync, RewritePlan, RewriteStats, write_aof_base};
pub use replay::replay_aof;
pub use shards_meta::{Routing, ShardsMeta, read_shards_meta, write_shards_meta};
pub use kevy_resp::{Argv, ArgvView};
pub use rewrite_fmt::dump_aof;
pub use snapshot_read::{
    load_snapshot, load_snapshot_filtered, load_snapshot_from, read_snapshot_cursor,
};
pub use snapshot_write::{
    save_snapshot, write_snapshot_tmp, write_snapshot_to, write_snapshot_to_with_cursor,
};
pub(crate) use rewrite_fmt::{dump_store_to_buf, estimate_multibulk_bytes, write_multibulk};
pub(crate) use snapshot_fmt::{SNAPSHOT_BUF_CAP, write_bytes};
pub(crate) use snapshot_write::write_stream_groups;
use kevy_store::Store;
use kevy_store::Value;

/// Anything that can enumerate `(key, &Value, ttl_ms)` triples for
/// serialization: a live [`Store`] (its `snapshot_each`, the synchronous
/// paths) or a frozen [`kevy_store::SnapshotView`] (the COW paths — collect
/// on the owning thread, serialize on a background one).
pub trait SnapshotSource {
    /// Visit every live entry as `(key, &value, remaining_ttl_ms)`.
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>));

    /// v2.4: visit every live hash field TTL as `(key, field,
    /// absolute_unix_ms)`. Default = none (sources predating the
    /// feature).
    fn for_each_hash_ttl(&self, _f: impl FnMut(&[u8], &[u8], u64)) {}
}

impl SnapshotSource for Store {
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.snapshot_each(f);
    }
    fn for_each_hash_ttl(&self, f: impl FnMut(&[u8], &[u8], u64)) {
        self.hash_ttl_each(f);
    }
}

impl SnapshotSource for kevy_store::SnapshotView {
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.each(f);
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
