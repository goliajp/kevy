//! Shard-lock + AOF-commit glue split from `store.rs` (500-LOC rule).
//! Import paths stay stable via the re-export in `store.rs`.

use crate::KevyResult;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(feature = "persist")]
use kevy_persist::Aof;
use kevy_store::StoreError;

use crate::store::Inner;

/// Write-lock an `Inner`, recovering from poison (short critical sections; a
/// panic in one doesn't corrupt the keyspace).
pub(crate) fn lock_write(shard: &RwLock<Inner>) -> RwLockWriteGuard<'_, Inner> {
    shard.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read-lock an `Inner`, recovering from poison.
pub(crate) fn lock_read(shard: &RwLock<Inner>) -> RwLockReadGuard<'_, Inner> {
    shard.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The caller's argument slices as an argv, so the log encodes from them
/// without an owned copy.
#[cfg(feature = "persist")]
struct Parts<'a, 'b>(&'a [&'b [u8]]);

#[cfg(feature = "persist")]
impl core::ops::Index<usize> for Parts<'_, '_> {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        self.0[i]
    }
}

#[cfg(feature = "persist")]
impl kevy_resp::ArgvView for Parts<'_, '_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        self.0.get(i).copied()
    }
}

#[cfg(feature = "persist")]
fn log_argv(aof: &mut Option<Aof>, parts: &[&[u8]]) -> KevyResult<()> {
    if let Some(aof) = aof {
        aof.append(&Parts(parts))?;
    }
    Ok(())
}

/// Whether a write on this shard is recorded anywhere a frame of it
/// would reach: the AOF, the embed-as-writer replication source, or the
/// change feed.
pub(crate) fn records_writes(inner: &Inner) -> bool {
    #[cfg(feature = "persist")]
    if inner.aof.is_some() {
        return true;
    }
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    if inner.writer_source.is_some() || inner.feed.is_some() {
        return true;
    }
    let _ = inner;
    false
}

/// Complete a write on one shard: AOF-log the canonical RESP command,
/// publish to the embed-as-writer replication source (if configured),
/// then run that shard's post-write eviction sweep.
#[cfg_attr(
    not(any(feature = "persist", feature = "replicate", feature = "index")),
    allow(unused_variables) // `parts` feeds the AOF / feed / index hooks
)]
pub(crate) fn commit_write(inner: &mut Inner, parts: &[&[u8]]) -> KevyResult<()> {
    #[cfg(feature = "persist")]
    log_argv(&mut inner.aof, parts)?;
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    if let Some(src) = &inner.writer_source {
        crate::replica_source::push_into(src, parts);
    }
    #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
    if let Some(feed) = &inner.feed {
        crate::store::Store::feed_push(feed, parts);
    }
    if let Some(b) = &inner.blocker {
        b.wake_all();
    }
    #[cfg(feature = "index")]
    if let Some(reg) = inner.idx_reg.clone() {
        let inner = &mut *inner;
        crate::ops_index::on_commit(&reg, &mut inner.idx_segs, &mut inner.store, parts);
    }
    #[cfg(feature = "index")]
    if let Some(vreg) = inner.view_reg.clone() {
        let inner = &mut *inner;
        crate::ops_view::on_commit(&vreg, &mut inner.view_segs, &mut inner.idx_segs, parts);
    }
    inner.store.try_evict_after_write();
    // The demotion twin (tiering): one budgeted spill batch when past
    // the watermark; a cheap not-taken branch when tiering is off.
    inner.store.try_demote_after_write();
    Ok(())
}

/// Record the absolute deadline `key` has now (`PEXPIREAT`), so a
/// relative TTL set a moment ago replays to the same instant. Nothing is
/// recorded when the key has no deadline.
pub(crate) fn commit_deadline(inner: &mut Inner, key: &[u8]) -> KevyResult<()> {
    let Some(f) = kevy_verbs::aof::deadline_frame(&mut inner.store, key) else {
        return Ok(());
    };
    let parts: Vec<&[u8]> = (0..f.len()).map(|i| &f[i]).collect();
    commit_write(inner, &parts)
}

pub(crate) fn store_err(e: StoreError) -> kevy_store::KevyError {
    kevy_store::KevyError::Store(e)
}
