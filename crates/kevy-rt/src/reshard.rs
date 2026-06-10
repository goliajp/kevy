//! Server-side shard-layout bring-up: detect a `shards.meta` mismatch and
//! re-home every key before any shard thread spawns.
//!
//! Per-shard `dump-{i}.rdb` / `aof-{i}.aof` files are only readable under
//! the (shard count, routing scheme) that wrote them. Until now the server
//! recorded neither — restarting with a different `--threads` silently
//! stranded keys in files no shard would route to (the embedded store fixed
//! this in its B2 sharding; the server never did). Cluster mode adds a
//! second axis (KevyHash vs slots routing), so both are now recorded and a
//! mismatch triggers one centralized, lossless re-shard, modeled on the
//! embedded path: merge every source file into a temp store, redistribute
//! under the new layout, back sources up as `.premigration.<nanos>`, write
//! fresh per-shard snapshots, record the layout.

use crate::Commands;
use crate::reduce::shard_of;
use kevy_persist::{
    Routing, ShardsMeta, load_snapshot, read_shards_meta, replay_aof, save_snapshot,
    write_shards_meta,
};
use kevy_store::Store;
use std::io;
use std::path::{Path, PathBuf};

/// Ensure `dir`'s persisted layout matches `(n, routing)`, re-sharding once
/// if it doesn't. Called by `Runtime::run` before any shard thread spawns;
/// afterwards each shard loads its own files exactly as before.
pub(crate) fn ensure_layout<C: Commands>(
    dir: &Path,
    n: usize,
    routing: Routing,
    commands: &C,
) -> io::Result<()> {
    let meta_path = dir.join("shards.meta");
    let target = ShardsMeta { n, routing };
    let prev = match read_shards_meta(&meta_path) {
        Some(m) => m,
        // Legacy dir (server never wrote meta): the shard count is however
        // many per-shard files exist, the routing is the only scheme that
        // existed. An empty dir trivially "matches" — just record target.
        None => ShardsMeta {
            n: infer_legacy_n(dir),
            routing: Routing::KevyHash,
        },
    };
    if prev.n == 0 || prev == target {
        std::fs::create_dir_all(dir)?;
        return write_shards_meta(&meta_path, target);
    }
    reshard(dir, prev, target, commands)?;
    write_shards_meta(&meta_path, target)
}

/// Whether `dir` holds any kevy persistence artifacts (per-shard snapshot,
/// AOF, or a `shards.meta`). Gates layout reconciliation for pure in-memory
/// runs so they keep writing nothing.
pub(crate) fn has_kevy_files(dir: &Path) -> bool {
    infer_legacy_n(dir) > 0 || dir.join("shards.meta").exists()
}

/// Highest `dump-{i}.rdb` / `aof-{i}.aof` index + 1, or 0 for no files.
fn infer_legacy_n(dir: &Path) -> usize {
    let mut n = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let idx = name
            .strip_prefix("dump-")
            .and_then(|r| r.strip_suffix(".rdb"))
            .or_else(|| name.strip_prefix("aof-").and_then(|r| r.strip_suffix(".aof")));
        if let Some(i) = idx.and_then(|s| s.parse::<usize>().ok()) {
            n = n.max(i + 1);
        }
    }
    n
}

/// Merge every `prev` source file into one temp store, redistribute under
/// `target`, back up the sources, write per-shard snapshots. AOFs are not
/// rewritten — the snapshot is the full state and each shard opens a fresh
/// (empty) log on bring-up; the old logs live on in the backups.
fn reshard<C: Commands>(
    dir: &Path,
    prev: ShardsMeta,
    target: ShardsMeta,
    commands: &C,
) -> io::Result<()> {
    let mut temp = Store::new();
    let mut sources: Vec<PathBuf> = Vec::new();
    for i in 0..prev.n {
        let snap = dir.join(format!("dump-{i}.rdb"));
        if snap.exists() {
            load_snapshot(&mut temp, &snap)?;
            sources.push(snap);
        }
        let aof = dir.join(format!("aof-{i}.aof"));
        if aof.exists() {
            replay_aof(&aof, |args| {
                commands.dispatch(&mut temp, &args);
            })?;
            sources.push(aof);
        }
    }

    let mut stores: Vec<Store> = (0..target.n).map(|_| Store::new()).collect();
    let slots = target.routing == Routing::Slots;
    temp.snapshot_each(|key, value, ttl_ms| {
        stores[shard_of(key, target.n, slots)].load_value(key, value, ttl_ms);
    });

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for src in &sources {
        let mut bak = src.clone().into_os_string();
        bak.push(format!(".premigration.{stamp}"));
        std::fs::rename(src, &bak)?;
    }
    for (i, store) in stores.iter().enumerate() {
        save_snapshot(store, &dir.join(format!("dump-{i}.rdb")))?;
    }
    eprintln!(
        "kevy: re-sharded {} -> {} shards ({:?} -> {:?} routing); {} source file(s) backed up as .premigration.{stamp}",
        prev.n, target.n, prev.routing, target.routing, sources.len(),
    );
    Ok(())
}
