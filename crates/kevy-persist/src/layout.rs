//! Per-shard persistence file naming — the single source of truth for the
//! on-disk layout. Every kevy data dir is a flat set of per-shard files:
//!
//! - `dump-{i}.rdb` — shard `i`'s snapshot ([`crate::save_snapshot`])
//! - `aof-{i}.aof` — shard `i`'s append-only log ([`crate::Aof`])
//! - `shards.meta` — the layout record ([`crate::ShardsMeta`])
//! - `LOCK` — the directory's advisory lock ([`crate::DirLock`])
//!
//! The server runtime, the embedded store, and the reshard engine all
//! derive their paths from here, so a dir written by one is readable by
//! the others (the embedded store's custom-filename opt-out aside).
//!
//! ```
//! use kevy_persist::layout;
//!
//! let dir = kevy_tmpdir::unique_dir("layout-doc");
//! kevy_persist::save_snapshot(&kevy_store::Store::new(), &layout::snapshot_path(&dir, 0))?;
//! std::fs::write(layout::aof_path(&dir, 1), kevy_persist::AOF2_MAGIC)?;
//! // a dir one writer laid out is read back by anything deriving the same names
//! assert_eq!(layout::infer_files_n(&dir), 2);
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), std::io::Error>(())
//! ```

use std::path::{Path, PathBuf};

/// Shard `i`'s snapshot file name.
///
/// ```
/// assert_eq!(kevy_persist::layout::snapshot_file(3), "dump-3.rdb");
/// ```
pub fn snapshot_file(i: usize) -> String {
    format!("dump-{i}.rdb")
}

/// Shard `i`'s AOF file name.
///
/// ```
/// assert_eq!(kevy_persist::layout::aof_file(3), "aof-3.aof");
/// ```
pub fn aof_file(i: usize) -> String {
    format!("aof-{i}.aof")
}

/// Shard `i`'s snapshot path under `dir`.
///
/// ```
/// use std::path::Path;
/// let p = kevy_persist::layout::snapshot_path(Path::new("/data"), 0);
/// assert_eq!(p, Path::new("/data/dump-0.rdb"));
/// ```
pub fn snapshot_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(snapshot_file(i))
}

/// Shard `i`'s AOF path under `dir`.
///
/// ```
/// use std::path::Path;
/// let p = kevy_persist::layout::aof_path(Path::new("/data"), 0);
/// assert_eq!(p, Path::new("/data/aof-0.aof"));
/// ```
pub fn aof_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(aof_file(i))
}

/// Shard `i`'s staging ring path under `dir`: the AOF's name plus
/// `.stage`.
///
/// ```
/// use std::path::Path;
/// use kevy_persist::layout::{aof_path, stage_path};
///
/// let ring = stage_path(Path::new("/data"), 2);
/// assert_eq!(ring, Path::new("/data/aof-2.aof.stage"));
/// assert_eq!(ring.with_extension(""), aof_path(Path::new("/data"), 2));
/// ```
pub fn stage_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{}.stage", aof_file(i)))
}

/// The advisory-lock file's path under `dir` ([`crate::DirLock`]).
///
/// ```
/// use std::path::Path;
/// assert_eq!(kevy_persist::layout::lock_path(Path::new("/data")), Path::new("/data/LOCK"));
/// ```
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join("LOCK")
}

/// The layout record's path under `dir`.
///
/// ```
/// use kevy_persist::{Routing, ShardsMeta, layout};
///
/// let dir = kevy_tmpdir::unique_dir("layout-meta-doc");
/// ShardsMeta::new(2, Routing::KevyHash).write(&layout::shards_meta_path(&dir))?;
/// assert!(dir.join("shards.meta").exists());
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn shards_meta_path(dir: &Path) -> PathBuf {
    dir.join("shards.meta")
}

/// Shard `i`'s cold-segment directory under `dir` — the segment files
/// plus the manifest that makes them real. Per shard, like the AOF it
/// stitches into: replay threads never share a manifest.
///
/// ```
/// use std::path::Path;
/// let d = kevy_persist::layout::segs_dir(Path::new("/data"), 1);
/// assert_eq!(d, Path::new("/data/segs-1"));
/// ```
pub fn segs_dir(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("segs-{i}"))
}

/// Highest `dump-{i}.rdb` / `aof-{i}.aof` index + 1 found in `dir`, or 0
/// for no per-shard files. The shard count of a meta-less legacy dir.
///
/// ```
/// use kevy_persist::layout;
///
/// let dir = kevy_tmpdir::unique_dir("layout-infer-doc");
/// assert_eq!(layout::infer_files_n(&dir), 0);
/// std::fs::write(dir.join(layout::aof_file(2)), b"")?; // shards 0 and 1 wrote nothing
/// assert_eq!(layout::infer_files_n(&dir), 3);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
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
