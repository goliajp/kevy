//! Per-shard persistence file naming — the single source of truth for the
//! on-disk layout. Every kevy data dir is a flat set of per-shard files:
//!
//! - `dump-{i}.rdb` — shard `i`'s snapshot ([`crate::save_snapshot`])
//! - `aof-{i}.aof` — shard `i`'s append-only log ([`crate::Aof`])
//! - `shards.meta` — the layout record ([`crate::ShardsMeta`])
//!
//! The server runtime, the embedded store, and the reshard engine all
//! derive their paths from here, so a dir written by one is readable by
//! the others (the embedded store's custom-filename opt-out aside).

use std::path::{Path, PathBuf};

/// Shard `i`'s snapshot file name.
pub fn snapshot_file(i: usize) -> String {
    format!("dump-{i}.rdb")
}

/// Shard `i`'s AOF file name.
pub fn aof_file(i: usize) -> String {
    format!("aof-{i}.aof")
}

/// Shard `i`'s snapshot path under `dir`.
pub fn snapshot_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(snapshot_file(i))
}

/// Shard `i`'s AOF path under `dir`.
pub fn aof_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(aof_file(i))
}

/// The layout record's path under `dir`.
pub fn shards_meta_path(dir: &Path) -> PathBuf {
    dir.join("shards.meta")
}

/// Highest `dump-{i}.rdb` / `aof-{i}.aof` index + 1 found in `dir`, or 0
/// for no per-shard files. The shard count of a meta-less legacy dir.
pub fn infer_files_n(dir: &Path) -> usize {
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
