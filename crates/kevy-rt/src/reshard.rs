//! Server-side shard-layout bring-up: detect a `shards.meta` mismatch and
//! re-home every key before any shard thread spawns.
//!
//! The crash-safe mechanics (temp snapshots → journal commit point →
//! finalize, with roll-forward recovery on the next start) live in
//! [`kevy_persist::reshard`] — shared with the embedded store. This file
//! only wires the server's specifics in: AOF frames replay through the full
//! command table, and keys redistribute under the server's routing
//! (KevyHash, or slot ranges in cluster mode).

use crate::Commands;
use crate::reduce::shard_of;
use kevy_persist::reshard::{StdLayout, commit_reshard, merge_sources, recover_journal};
use kevy_persist::{Routing, ShardsMeta, layout, read_shards_meta, write_shards_meta};
use kevy_store::Store;
use std::io;
use std::path::Path;

/// Ensure `dir`'s persisted layout matches `(n, routing)`, re-sharding once
/// if it doesn't. Called by `Runtime::run` before any shard thread spawns;
/// afterwards each shard loads its own files exactly as before. A reshard
/// interrupted by a crash is completed (or safely discarded) first.
pub(crate) fn ensure_layout<C: Commands>(
    dir: &Path,
    n: usize,
    routing: Routing,
    commands: &C,
) -> io::Result<()> {
    let meta_path = layout::shards_meta_path(dir);
    recover_journal(dir, &StdLayout)?;
    let target = ShardsMeta { n, routing };
    let prev = match read_shards_meta(&meta_path) {
        Some(m) => m,
        // Legacy dir (server never wrote meta): the shard count is however
        // many per-shard files exist, the routing is the only scheme that
        // existed. An empty dir trivially "matches" — just record target.
        None => ShardsMeta {
            n: layout::infer_files_n(dir),
            routing: Routing::KevyHash,
        },
    };
    if prev.n == 0 || prev == target {
        std::fs::create_dir_all(dir)?;
        return write_shards_meta(&meta_path, target);
    }
    reshard(dir, prev, target, commands)
}

/// Whether `dir` holds any kevy persistence artifacts (per-shard snapshot,
/// AOF, or a `shards.meta`). Gates layout reconciliation for pure in-memory
/// runs so they keep writing nothing.
pub(crate) fn has_kevy_files(dir: &Path) -> bool {
    layout::infer_files_n(dir) > 0 || layout::shards_meta_path(dir).exists()
}

/// Merge every `prev` source file into one temp store (AOF frames replayed
/// through the command table), redistribute under `target`'s routing, then
/// hand the crash-safe commit to the engine — which also records the new
/// layout in `shards.meta`.
fn reshard<C: Commands>(
    dir: &Path,
    prev: ShardsMeta,
    target: ShardsMeta,
    commands: &C,
) -> io::Result<()> {
    let mut temp = Store::new();
    let sources = merge_sources(dir, prev.n, &StdLayout, &mut temp, |store, args| {
        commands.dispatch(store, &args);
    })?;

    let mut stores: Vec<Store> = (0..target.n).map(|_| Store::new()).collect();
    let slots = target.routing == Routing::Slots;
    temp.snapshot_each(|key, value, ttl_ms| {
        stores[shard_of(key, target.n, slots)].load_value(key, value, ttl_ms);
    });

    let stamp = commit_reshard(dir, prev.n, target, &stores, &StdLayout)?;
    eprintln!(
        "kevy: re-sharded {} -> {} shards ({:?} -> {:?} routing); {} source file(s) backed up as .premigration.{stamp}",
        prev.n, target.n, prev.routing, target.routing, sources.len(),
    );
    Ok(())
}
