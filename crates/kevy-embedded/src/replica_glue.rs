//! Glue between the public `Store` surface and the replica runner —
//! the constructor that decides whether to spawn the background
//! thread, plus the read-only enforcement helper that every mutating
//! API in `ops.rs` calls. Extracted from `store.rs` to keep that file
//! under the 500-LOC ceiling.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Config;
use crate::store::{Shards, Store};

/// Construct + spawn the replica runner when configured. Returns
/// `None` when the upstream is unset (normal primary store). The
/// returned handle is owned by `DropGuard`, which joins the runner
/// thread on the last `Store` clone drop.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn_replica_runner(
    config: &Config,
    shards: &Shards,
) -> Option<crate::replica_runner::ReplicaRunner> {
    let upstream = config.replica_upstream.as_ref()?.clone();
    Some(crate::replica_runner::ReplicaRunner::spawn(
        shards.clone(),
        upstream,
        config.replica_id.clone(),
        config.replica_reconnect_min,
        config.replica_reconnect_max,
    ))
}

/// Generate a process-unique replica id for `Store::open_replica`.
/// Format: `"kevy-embedded-{pid}-{seq}"` — the pid stays stable across
/// a process, the seq counter advances per open so two embeds in the
/// same process don't collide on a single slot in the primary's
/// SlotTable (the bug that would otherwise cause backlog frames a
/// fresh replica still needs to be evicted as soon as the prior
/// embed disconnects).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fresh_replica_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("kevy-embedded-{}-{}", std::process::id(), n)
}

/// Read-only enforcement for replica stores. Returns `READONLY ...`
/// on a replica; `Ok(())` on a primary. Called at the top of every
/// mutating public API in `ops.rs`. The error message intentionally
/// mirrors the server-side wire string (`-READONLY ...`) so
/// applications can pattern-match the same way on both backends.
pub(crate) fn ensure_writable(store: &Store) -> io::Result<()> {
    if store.is_replica() {
        return Err(io::Error::other(
            "READONLY You can't write against a kevy-embedded replica",
        ));
    }
    Ok(())
}
