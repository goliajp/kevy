//! File-backed rewrite tee (S5-G): during an offload-mode rewrite the
//! diff no longer accumulates as anonymous memory — records land in a
//! bounded staging buffer, the io_uring driver ships staging chunks as
//! positioned writes into `<aof>.tee`, and the persist worker folds
//! completed ranges into the tmp image file-to-file. The in-memory Vec
//! tee measured as GB/s of anonymous allocation under a saturated
//! firehose, which drove the box into direct reclaim inside the
//! reactor's own fault paths (the S5-E/F finding); this keeps the
//! reactor's rewrite footprint at one recycled staging buffer.
//!
//! Crash story: the live AOF still receives every write through the
//! normal append path, so `<aof>.tee` needs no fsync ever — a crash
//! discards the whole rewrite attempt, and the next attempt's
//! truncating `File::create` reclaims any orphan (same contract as the
//! `.rewrite` tmp itself).

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One live file-backed tee. Offsets are file positions:
/// `consumed ≤ handed ≤ handed + staged.len()` is the byte stream's
/// invariant — `[0, consumed)` already folded into the tmp image,
/// `[consumed, handed)` on disk (or in flight on the ring, which the
/// driver's structural gate drains before any fold/finish), and
/// `staged` not yet handed to the driver.
pub(crate) struct TeeFile {
    pub(crate) file: File,
    pub(crate) path: PathBuf,
    /// Records not yet handed to the ring driver.
    pub(crate) staged: Vec<u8>,
    /// File offset up to which staging chunks have been handed off.
    pub(crate) handed: u64,
    /// File offset up to which the worker has folded bytes into tmp.
    pub(crate) consumed: u64,
}

impl TeeFile {
    /// Truncating-create `<aof>.tee` beside the log (same directory, so
    /// everything the rewrite touches shares the filesystem). Read AND
    /// write: the ring writes at positions, the fold and the final
    /// swap read ranges back — a write-only handle EBADFs the fold
    /// (caught by the round-trip test).
    pub(crate) fn create(aof_path: &Path) -> io::Result<Self> {
        let path = tee_path(aof_path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            staged: Vec::new(),
            handed: 0,
            consumed: 0,
        })
    }

    /// Total diff bytes not yet folded into the image — the overrun
    /// check's and the convergence test's size.
    pub(crate) fn lag(&self) -> u64 {
        (self.handed - self.consumed).saturating_add(self.staged.len() as u64)
    }
}

/// `<aof>.tee` — the file-backed diff beside the log.
pub(crate) fn tee_path(aof_path: &Path) -> PathBuf {
    let mut p = aof_path.to_path_buf();
    let name = match aof_path.file_name() {
        Some(n) => {
            let mut s = n.to_os_string();
            s.push(".tee");
            s
        }
        None => std::ffi::OsString::from("aof.tee"),
    };
    p.set_file_name(name);
    p
}

use crate::aof::{Aof, RewriteStats};

impl Aof {
    /// Phase 1 of an offload-mode COW rewrite: like
    /// [`Self::begin_view_rewrite`] but the diff goes to `<aof>.tee`
    /// through the ring instead of an in-memory Vec. Same atomicity
    /// contract (no `append` between the view collect and this call).
    pub fn begin_view_rewrite_filetee(&mut self) -> io::Result<PathBuf> {
        self.flush_queued()?;
        self.file.flush()?;
        let mut tf = TeeFile::create(&self.path)?;
        // Recycle the pooled staging buffer across rewrites.
        if let Some(spare) = self.tee_spare.take() {
            tf.staged = spare;
        }
        self.tee_file = Some(tf);
        Ok(crate::aof_util::rewrite_tmp_path(&self.path))
    }

    /// Staging → one ring chunk `(file_offset, bytes, fd)`; `None` when
    /// nothing is staged or the tee is not file-backed. The driver owns
    /// the bytes until the CQE, then recycles them via
    /// [`Self::stash_tee_spare`].
    #[cfg(unix)]
    pub fn take_tee_pending(&mut self) -> Option<(u64, Vec<u8>, std::os::fd::RawFd)> {
        use std::os::fd::AsRawFd;
        let tf = self.tee_file.as_mut()?;
        if tf.staged.is_empty() {
            return None;
        }
        let spare = self.tee_spare.take().unwrap_or_default();
        let chunk = std::mem::replace(&mut tf.staged, spare);
        let offset = tf.handed;
        tf.handed += chunk.len() as u64;
        Some((offset, chunk, tf.file.as_raw_fd()))
    }

    /// `(consumed, handed)` fold watermarks; `None` off file-tee mode.
    /// Only call with the ring drained of tee chunks (the structural
    /// gate) — `handed` counts handed-off bytes, durable-in-file only
    /// once their CQEs landed.
    pub fn tee_watermarks(&self) -> Option<(u64, u64)> {
        self.tee_file.as_ref().map(|t| (t.consumed, t.handed))
    }

    /// The worker folded `[consumed, to)` into the tmp image.
    pub fn tee_advance_consumed(&mut self, to: u64) {
        if let Some(t) = &mut self.tee_file {
            t.consumed = t.consumed.max(to);
        }
    }

    /// An independent handle on `<aof>.tee` for the worker's fold
    /// (reads never race the ring's positioned writes: the fold range
    /// is bounded by `handed` at a drained moment).
    pub fn tee_copy_handle(&self) -> Option<io::Result<File>> {
        self.tee_file.as_ref().map(|t| t.file.try_clone())
    }

    /// Final swap, file-tee mode: push the staging remainder straight
    /// into the tee file, fold the ≤SMALL_TEE tail into the tmp image,
    /// then the shared swap. Returns the stats and the `.tee` path for
    /// an off-thread unlink.
    #[cfg(unix)]
    pub fn finish_concurrent_rewrite_from_tee(
        &mut self,
        tmp: &Path,
        keys: u64,
    ) -> io::Result<(RewriteStats, PathBuf)> {
        use std::io::Read;
        use std::os::unix::fs::FileExt;
        let Some(tf) = self.tee_file.take() else {
            return Err(io::Error::other("no file tee live"));
        };
        // Staging remainder lands at `handed` (the ring is drained —
        // structural gate — so every earlier byte is in the file).
        if !tf.staged.is_empty() {
            tf.file.write_all_at(&tf.staged, tf.handed)?;
        }
        let end = tf.handed + tf.staged.len() as u64;
        {
            let mut dst = std::fs::OpenOptions::new().append(true).open(tmp)?;
            let mut src = tf.file.try_clone()?;
            use std::io::Seek;
            src.seek(io::SeekFrom::Start(tf.consumed))?;
            let mut take = src.take(end - tf.consumed);
            io::copy(&mut take, &mut dst)?;
            dst.flush()?;
            dst.sync_all()?;
        }
        // Recycle the staging buffer for the next rewrite's tee.
        let mut staged = tf.staged;
        staged.clear();
        self.stash_tee_spare(staged);
        let stats = self.swap_image(tmp, keys)?;
        Ok((stats, tf.path))
    }

    /// Tear down file-tee mode: recycle staging, hand back the `.tee`
    /// path so the caller unlinks it off-thread. The file handle drops
    /// here (close is cheap; the unlink is what contends the journal).
    pub fn take_tee_file_teardown(&mut self) -> Option<PathBuf> {
        let tf = self.tee_file.take()?;
        let mut staged = tf.staged;
        staged.clear();
        self.stash_tee_spare(staged);
        Some(tf.path)
    }
}
