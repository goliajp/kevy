//! The concurrent (non-blocking) rewrite family of [`Aof`] — begin /
//! tee handoff / finish / abort — split from `aof.rs` at the 500-LOC
//! line when the two-phase handoff grew it past the cap. The two-phase
//! protocol's driver lives in kevy-rt (`advance_rewrite_handoff`); this
//! file owns the Aof-side state transitions.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::aof::{AOF_BUF_CAP, Aof, RewritePlan, RewriteStats};
use crate::dump_store_to_buf;
use kevy_store::Store;

impl Aof {
    /// Phase 1 of a **non-blocking** rewrite (Background auto-rewrite). Must be
    /// called under the store lock: it serializes the keyspace into an
    /// in-memory image and starts teeing subsequent `append`s into a diff
    /// buffer — both atomic w.r.t. other writes. The caller then spills
    /// `plan.body` to `plan.tmp` **with the lock released** (the slow disk
    /// write), and finally calls [`Self::finish_concurrent_rewrite`] under the
    /// lock again. Writes that land during the off-lock spill are captured by
    /// the tee and appended after the snapshot, so nothing is lost.
    pub fn begin_concurrent_rewrite(&mut self, store: &Store) -> io::Result<RewritePlan> {
        self.flush_queued()?;
        let (body, keys) = dump_store_to_buf(store, crate::AofFormat::V2);
        self.rewrite_tee = Some(Vec::new());
        Ok(RewritePlan { body, tmp: crate::aof_util::rewrite_tmp_path(&self.path), keys })
    }

    /// Phase 2: the `plan.body` is already on disk at `tmp` (spilled off-lock).
    /// Append the diff buffer (writes since `begin`), fsync, atomically swap
    /// over the live AOF, and reopen the append handle against it. Call under
    /// the store lock. `keys` is `plan.keys`.
    pub fn finish_concurrent_rewrite(&mut self, tmp: &Path, keys: u64) -> io::Result<RewriteStats> {
        let tee = self.rewrite_tee.take().unwrap_or_default();
        // Embedded/synchronous path: the caller's own thread is not a
        // reactor, so dropping the spent buffers inline here is fine.
        self.finish_concurrent_rewrite_with(tmp, keys, tee).map(|(stats, _bufs)| stats)
    }

    /// Take the accumulated diff buffer for an off-thread append and
    /// immediately start a fresh generation — the two-phase rewrite's
    /// handoff step (the worker appends+fsyncs THIS tee while new writes
    /// keep teeing into the fresh one). `None` when no rewrite is live.
    /// `is_rewriting` stays true throughout. The fresh generation reuses
    /// the recycled spare buffer when one is stashed (warm pages, no
    /// fresh mmap — see `stash_tee_spare`).
    pub fn take_tee_for_handoff(&mut self) -> Option<Vec<u8>> {
        let spare = self.tee_spare.take().unwrap_or_default();
        match self.rewrite_tee.as_mut() {
            Some(t) => Some(std::mem::replace(t, spare)),
            None => {
                self.tee_spare = Some(spare);
                None
            }
        }
    }

    /// Live diff-buffer size, `None` when no rewrite is running — the
    /// tick's overrun check reads this to defer a diverging rewrite
    /// while the tee is still growing (before the gigabytes, not after).
    #[must_use]
    pub fn tee_len(&self) -> Option<usize> {
        self.rewrite_tee.as_ref().map(Vec::len)
    }

    /// Return an appended generation's buffer (cleared) for the next
    /// generation to grow into. Ping-pong: at most one generation is
    /// ever out with the worker, so the slot is normally empty; a
    /// surprise second return is kept only if larger. The LOSING buffer
    /// is returned to the caller instead of dropped here — both
    /// ping-pong buffers carry the FIRST generation's capacity forever
    /// (a clear never shrinks), so "the smaller one" can still be
    /// gigabytes, and a GB munmap on the reactor was the finish-tick
    /// stall the diag timers caught at 288 ms. The caller ships it to
    /// the worker like every other GB-scale free.
    #[must_use]
    pub fn stash_tee_spare(&mut self, mut buf: Vec<u8>) -> Option<Vec<u8>> {
        buf.clear();
        match &self.tee_spare {
            Some(held) if held.capacity() >= buf.capacity() => Some(buf),
            _ => self.tee_spare.replace(buf),
        }
    }

    /// Drain every retained tee buffer (the live diff and the recycled
    /// spare) for an off-thread drop at rewrite teardown — freeing a
    /// GB-scale buffer is a munmap that must not run on the reactor.
    /// Call BEFORE `abort_concurrent_rewrite` (which drops inline).
    pub fn take_tee_teardown(&mut self) -> Vec<Vec<u8>> {
        let mut bufs = Vec::new();
        if let Some(t) = self.rewrite_tee.take() {
            bufs.push(t);
        }
        if let Some(s) = self.tee_spare.take() {
            bufs.push(s);
        }
        bufs.retain(|b| b.capacity() > 0);
        bufs
    }

    /// Phase-final of the two-phase rewrite: `last_tee` is the (small)
    /// final diff generation; everything earlier was appended+fsynced
    /// to `tmp` off-thread. The synchronous cost here is bounded by the
    /// handoff window's writes, not the rewrite window's — `sync_all`
    /// pays for dirty pages, and the worker already flushed the image
    /// and the earlier tee generations.
    pub fn finish_concurrent_rewrite_with(
        &mut self,
        tmp: &Path,
        keys: u64,
        tee: Vec<u8>,
    ) -> io::Result<(RewriteStats, Vec<Vec<u8>>)> {
        // Both the live tee and the passed generation can carry the
        // FIRST generation's GB-scale capacity (clears never shrink);
        // hand them back to the caller instead of dropping here — on
        // the reactor a GB munmap is the finish-tick stall the diag
        // timers measured at 288 ms.
        let mut spent: Vec<Vec<u8>> = self.rewrite_tee.take().into_iter().collect();
        {
            let mut f = OpenOptions::new().append(true).open(tmp)?;
            f.write_all(&tee)?;
            f.sync_all()?;
        }
        spent.push(tee);
        std::fs::rename(tmp, &self.path)?;
        let f = OpenOptions::new().append(true).open(&self.path)?;
        let bytes = f.metadata().map_or(0, |m| m.len());
        self.file = BufWriter::with_capacity(AOF_BUF_CAP, f);
        self.format = crate::AofFormat::V2; // the rewrite output always is
        self.size_bytes = bytes;
        self.queued_offset = bytes;
        self.size_at_last_rewrite = bytes;
        self.last_rewrite_at = Instant::now();
        self.dirty = false;
        self.rewrites_total = self.rewrites_total.saturating_add(1);
        Ok((RewriteStats { keys, bytes }, spent))
    }

    /// The graveyard name for the NEXT swap (queued mode only): the
    /// pre-swap log keeps a link so `rename(2)` never frees a multi-GB
    /// inode's extents inside the syscall.
    #[must_use]
    pub fn swap_trash_name(&self) -> Option<PathBuf> {
        self.queue.as_ref()?;
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("aof"));
        name.push(format!(".trash{}", self.rewrites_total));
        Some(self.path.with_file_name(name))
    }

    /// The live log's path — the off-thread swap job's rename target.
    #[must_use]
    pub fn live_path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Enter the swap-hold window: the worker is about to hardlink +
    /// rename over the live path, so the offload driver must stop
    /// draining the append queue (bytes keep accumulating in it —
    /// bounded by the hold's duration) and must not fsync the old fd.
    pub fn begin_swap_hold(&mut self) {
        self.swap_hold = true;
    }

    /// Whether the swap-hold window is open (the driver's queue gate).
    #[must_use]
    pub fn swap_holding(&self) -> bool {
        self.swap_hold
    }

    /// The reactor's half of an off-thread swap: the worker already
    /// renamed the image over the live path (journal work, done off
    /// this thread); reopen the append handle against it, reset every
    /// anchor, release the hold. Opening an EXISTING file writes no
    /// journal metadata — the reactor's synchronous cost is µs.
    pub fn swap_finalize_reopen(
        &mut self,
        keys: u64,
        trash: Option<PathBuf>,
    ) -> io::Result<RewriteStats> {
        self.swap_hold = false;
        // Deliberately NOT clearing `rewrite_tee` here: the buffer can
        // carry a GB-scale capacity (clears never shrink it), and `=
        // None` was an inline munmap on the reactor. The caller drains
        // it via `take_tee_teardown` immediately after this returns and
        // ships it to the worker; `is_rewriting` stays true for those
        // few statements on the same single-threaded reactor, which
        // nothing observes in between.
        self.swap_trash = trash;
        let f = OpenOptions::new().append(true).open(&self.path)?;
        let bytes = f.metadata().map_or(0, |m| m.len());
        self.file = BufWriter::with_capacity(AOF_BUF_CAP, f);
        self.format = crate::AofFormat::V2;
        self.size_bytes = bytes;
        self.queued_offset = bytes;
        self.size_at_last_rewrite = bytes;
        self.last_rewrite_at = Instant::now();
        self.dirty = false;
        self.rewrites_total = self.rewrites_total.saturating_add(1);
        Ok(RewriteStats { keys, bytes })
    }

    /// Abort an off-thread swap that failed before the rename landed:
    /// the live path still names the old log — resume appends against
    /// it unchanged.
    pub fn abort_swap_hold(&mut self) {
        self.swap_hold = false;
    }

    /// The graveyard hardlink from the last swap, for the caller's
    /// off-thread unlink. `None` outside queued mode (the epoll path
    /// keeps the classic inline drop — pre-existing behavior).
    pub fn take_swap_trash(&mut self) -> Option<PathBuf> {
        self.swap_trash.take()
    }

    /// Abandon an in-flight non-blocking rewrite (e.g. the off-lock spill
    /// failed): drop the diff buffer and resume normal appends. The live AOF
    /// is untouched, so no data is at risk; the caller deletes the temp file.
    pub fn abort_concurrent_rewrite(&mut self) {
        self.rewrite_tee = None;
    }

    /// A rewrite was aborted because ingest outran the disk (the tee
    /// generations stopped shrinking — see the two-phase driver in
    /// kevy-rt). Re-anchor the auto-rewrite growth rule at the CURRENT
    /// size, as if a rewrite had landed here: retrying immediately would
    /// diverge again identically, so the next attempt waits for another
    /// full growth factor (or an explicit BGREWRITEAOF). Degradation,
    /// not a stall: under sustained overload the log grows and the
    /// reactor stays responsive.
    pub fn anchor_rewrite_deferred(&mut self) {
        self.size_at_last_rewrite = self.size_bytes;
        self.last_rewrite_at = Instant::now();
    }

    /// Phase 1 of a **COW** rewrite: flush pending appends and start teeing
    /// subsequent ones into the diff buffer. O(1) — the keyspace itself is
    /// already frozen in the caller's `SnapshotView`. Returns the temp path
    /// the background serializer must write (via [`crate::dump_aof`]),
    /// after which [`Self::finish_concurrent_rewrite`] (same thread as the
    /// appends) swaps it in, or [`Self::abort_concurrent_rewrite`] backs out.
    ///
    /// **Atomicity contract**: the `collect_snapshot` and this call must
    /// happen with no `append` between them (same critical section / same
    /// thread). A write squeezing in between would either miss the new AOF
    /// (tee started late) or replay twice (tee started early) — and
    /// commands like LPUSH are not idempotent.
    pub fn begin_view_rewrite(&mut self) -> io::Result<std::path::PathBuf> {
        self.flush_queued()?;
        self.file.flush()?;
        self.rewrite_tee = Some(Vec::new());
        Ok(crate::aof_util::rewrite_tmp_path(&self.path))
    }
}
