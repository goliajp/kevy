//! File-level AOF utilities — the quarantine copy, the fresh rewrite
//! base, and the rewrite temp path. Split from `aof.rs` for the 500-LOC
//! house rule; behaviour unchanged.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Copy `[from, EOF)` of `path` to `<path>.corrupt-quarantine.<unix_ts>`
/// (fsynced) and return the quarantine path. Called before the torn/corrupt
/// tail is truncated away, so the dropped bytes stay inspectable — replay
/// never re-applies them. Errors propagate: failing to preserve the bytes
/// (e.g. disk full) fails the open with the file intact, which beats
/// destroying the only copy; free space and reopen.
pub(crate) fn quarantine_dropped_tail(path: &Path, from: u64) -> io::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let qpath = PathBuf::from(format!("{}.corrupt-quarantine.{ts}", path.display()));
    let mut src = File::open(path)?;
    src.seek(SeekFrom::Start(from))?;
    let mut dst = File::create(&qpath)?;
    io::copy(&mut src, &mut dst)?;
    dst.sync_data()?;
    Ok(qpath)
}

/// Write a fresh AOF base at `path`: just the magic header, fsynced. The
/// COW background-save's log reset starts from this — the post-collect
/// tee'd writes are appended by `finish_concurrent_rewrite` and the result
/// swaps over the live AOF (the snapshot now carries the pre-collect state).
pub fn write_aof_base(path: &Path) -> io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(crate::record::AOF2_MAGIC)?;
    f.sync_all()
}

/// `<aof>.rewrite` — same-directory temp path so `rename(2)` stays atomic.
pub(crate) fn rewrite_tmp_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let new_name = match path.file_name() {
        Some(n) => {
            let mut s = n.to_os_string();
            s.push(".rewrite");
            s
        }
        None => std::ffi::OsString::from("aof.rewrite"),
    };
    p.set_file_name(new_name);
    p
}

/// The tail-repair half of [`crate::Aof::open_with_repair`]. A crash —
/// power loss, or a VM/process kill with un-fsynced `EverySec`
/// pages — can leave a partial frame, or a zero-filled region,
/// after the last complete one. `replay_aof` stops there
/// (prefix-only), but an O_APPEND write would land *after* the
/// garbage, orphaning it behind the torn frame on the next replay:
/// silent data loss. Truncate to the last complete frame so
/// appends stay contiguous with the replayable prefix (Redis
/// `aof-load-truncated`) — but FIRST copy the dropped region
/// aside: after a corrupt frame MID-file the region is mostly
/// well-formed frames (a real incident dropped 231 MB over a
/// few-KB bad frame), and the forensic copy is the only way to
/// ever get them back.
pub(crate) fn repair_tail(
    path: &Path,
    file: &mut File,
    size: &mut u64,
    resync: bool,
) -> io::Result<Option<PathBuf>> {
    let valid = crate::replay::valid_prefix_len_of_file(path, resync)?;
    if valid >= *size {
        return Ok(None);
    }
    let q = crate::aof_util::quarantine_dropped_tail(path, valid)?;
    file.set_len(valid)?;
    file.sync_data()?;
    eprintln!(
        "kevy WARN: AOF {} dropped a non-replayable tail: {} bytes \
         quarantined to {} then truncated at byte {valid}.",
        path.display(),
        *size - valid,
        q.display(),
    );
    *size = valid;
    Ok(Some(q))
}
