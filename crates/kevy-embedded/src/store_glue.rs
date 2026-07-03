//! Shard-lock + AOF-commit glue split from `store.rs` (500-LOC rule).
//! Import paths stay stable via the re-export in `store.rs`.

use std::io;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use kevy_persist::{Aof, Argv};
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
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(feed) = &inner.feed {
        crate::store::Store::feed_push(feed, parts);
    }
    inner.store.try_evict_after_write();
    Ok(())
}

pub(crate) fn store_err(e: StoreError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("kevy-store: {e:?}"))
}
