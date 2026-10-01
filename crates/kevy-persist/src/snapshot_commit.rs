//! Committing a snapshot and the log reset that goes with it, so that a
//! restart after a crash at any point restores each write exactly once.

#![expect(clippy::let_underscore_must_use, reason = "removing what is already meant to be gone")]

use std::io::{self, Write};
use std::path::Path;

use crate::log_base::{LogHead, base_frame, prev_path, read_log_head, snapshot_id};
use crate::{Aof, RewriteStats};

/// Write a fresh log base at `path`: the magic and the frame naming the
/// snapshot the log continues, fsynced. The tee'd writes are appended to
/// it by `finish_concurrent_rewrite`.
pub(crate) fn write_log_base(path: &Path, snapshot_id: u64) -> io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(crate::record::AOF2_MAGIC)?;
    crate::record::write_record_multibulk(&mut f, &base_frame(snapshot_id), &mut Vec::new())?;
    f.sync_all()
}

impl Aof {
    /// Commit a background snapshot and this log's reset as one step.
    ///
    /// `snap_tmp` is the durable snapshot a save wrote
    /// ([`crate::write_snapshot_tmp`]) from a view frozen together with
    /// [`Self::begin_view_rewrite`]; `reset_tmp` is the path that call
    /// returned. The snapshot replaces `snapshot` and the log restarts
    /// with the writes teed since the freeze, naming the snapshot it
    /// continues. Until the new log is in place the replaced snapshot is
    /// kept beside it, so [`crate::settle_snapshot`] can restore a
    /// matching pair after a crash between the two renames.
    ///
    /// # Errors
    ///
    /// A failed step undoes the earlier ones: the previous snapshot and
    /// the live log stay as they were, and the tee is dropped.
    ///
    /// ```
    /// use kevy_persist::{Aof, Fsync, settle_snapshot, write_snapshot_tmp};
    /// use kevy_store::{SetCondition, Store};
    ///
    /// let dir = kevy_tmpdir::unique_dir("commit-snapshot-doc");
    /// let (snap, log) = (dir.join("dump-0.rdb"), dir.join("aof-0.aof"));
    /// let mut store = Store::new();
    /// store.set(b"k", b"v".to_vec(), None, SetCondition::Always);
    /// let mut aof = Aof::open(&log, Fsync::No)?;
    /// let reset = aof.begin_view_rewrite()?;
    /// let tmp = write_snapshot_tmp(&store, &snap)?;
    /// aof.commit_snapshot(&tmp, &snap, &reset)?;
    /// // the log now continues exactly this snapshot
    /// assert!(settle_snapshot(&snap, Some(&log))?);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn commit_snapshot(
        &mut self,
        snap_tmp: &Path,
        snapshot: &Path,
        reset_tmp: &Path,
    ) -> io::Result<RewriteStats> {
        let result = self.commit_snapshot_steps(snap_tmp, snapshot, reset_tmp);
        if result.is_err() {
            self.abort_concurrent_rewrite();
            let _ = std::fs::remove_file(reset_tmp);
        }
        result
    }

    fn commit_snapshot_steps(
        &mut self,
        snap_tmp: &Path,
        snapshot: &Path,
        reset_tmp: &Path,
    ) -> io::Result<RewriteStats> {
        let id = snapshot_id(snap_tmp)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "snapshot to commit carries no id")
        })?;
        let prev = prev_path(snapshot);
        let kept = match std::fs::rename(snapshot, &prev) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if let Err(e) = std::fs::rename(snap_tmp, snapshot) {
            return Err(undo(e, kept.then_some(&prev), snapshot, false));
        }
        let stats = match write_log_base(reset_tmp, id)
            .and_then(|()| self.finish_concurrent_rewrite(reset_tmp, 0))
        {
            Ok(stats) => stats,
            // the swap may have landed before a later step failed; then
            // the new log already continues the new snapshot
            Err(e) if read_log_head(&self.live_path())? == LogHead::After(id) => return Err(e),
            Err(e) => return Err(undo(e, kept.then_some(&prev), snapshot, true)),
        };
        if kept {
            // committed; a leftover is settled at the next restore
            let _ = std::fs::remove_file(&prev);
        }
        Ok(stats)
    }
}

/// Put back the snapshot the live log continues: the kept previous one,
/// or none when the new one was the first.
fn undo(e: io::Error, prev: Option<&Path>, snapshot: &Path, placed: bool) -> io::Error {
    let back = match prev {
        Some(prev) => std::fs::rename(prev, snapshot),
        None if placed => std::fs::remove_file(snapshot),
        None => Ok(()),
    };
    match back {
        Ok(()) => e,
        Err(b) => io::Error::new(e.kind(), format!("{e}; restoring the previous snapshot: {b}")),
    }
}
