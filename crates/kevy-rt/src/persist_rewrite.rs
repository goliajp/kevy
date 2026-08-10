//! The two-phase AOF rewrite's completion half on [`Shard`]: applying
//! the worker's `Rewrite` / `TeeAppend` results and driving the tee
//! handoff loop. Split from `persist_worker.rs` at the 500-LOC line —
//! the Aof-side state transitions live in kevy-persist's
//! `aof_rewrite.rs`; this file owns the reactor-side protocol.

use crate::Commands;
use crate::persist_worker::{PersistDone, PersistJob};
use crate::shard::Shard;

/// A tee past this size defers the rewrite at once: 64× the swap
/// bound cannot shrink to `SMALL_TEE` while ingest continues, and
/// letting it keep growing is the damage itself — the tee's GB/s
/// anonymous allocation is what pushed the box into direct reclaim
/// (5-6.5M pages scanned vs 6-18k without a rewrite; the S5-E/F
/// finding), stalling reactor faults on the LRU locks. Checked on the
/// tick WHILE the tee grows and again at each handoff step.
pub(crate) const TEE_DEFER_CAP: usize = 256 << 20;

/// In-flight two-phase handoff: the worker has spilled the image to
/// `tmp`; tee generations are being appended off-thread until they
/// converge (or provably can't).
pub(crate) struct RewriteHandoff {
    pub(crate) tmp: std::path::PathBuf,
    pub(crate) keys: u64,
    /// Generations already handed to the worker.
    pub(crate) iters: u8,
    /// Previous generation's size — the convergence test's memory.
    pub(crate) prev_len: usize,
}

impl<C: Commands> Shard<C> {
    /// Apply a rewrite-family completion (the `Rewrite` / `TeeAppend`
    /// arms of `commit_persist_done` — `Save` stays there).
    #[cold]
    pub(crate) fn commit_rewrite_done(&mut self, done: PersistDone) {
        match done {
            PersistDone::Rewrite {
                result: Ok(keys),
                tmp,
            } => {
                // Stale completion: the tick's overrun check deferred
                // this rewrite while the image was still dumping. The
                // diff is gone with the tee — swapping would lose it.
                if !self.aof.as_ref().is_some_and(kevy_persist::Aof::is_rewriting) {
                    self.abort_rewrite_cleanup(&tmp);
                    return;
                }
                self.rewrite_handoff = Some(RewriteHandoff {
                    tmp,
                    keys,
                    iters: 0,
                    prev_len: usize::MAX,
                });
                self.advance_rewrite_handoff();
            }
            PersistDone::TeeAppend { result, tmp, buf } => {
                self.on_tee_appended(result, &tmp, buf);
            }
            PersistDone::Cleanup { failed } => self.note_cleanup_failures(failed),
            PersistDone::TeeCopy { result, tmp, to } => self.on_tee_copied(result, tmp, to),
            PersistDone::Rewrite {
                result: Err(e),
                tmp,
            } => {
                eprintln!("kevy: shard {} aof rewrite failed: {e}", self.id);
                self.abort_rewrite_cleanup(&tmp);
            }
            // By-argument unreachable (the caller's match keeps Save in
            // commit_persist_done): fall back loudly rather than panic —
            // a dropped Save completion must not tear the shard down.
            PersistDone::Save { .. } => {
                eprintln!(
                    "kevy: shard {} Save completion routed to rewrite arm — dropped",
                    self.id
                );
            }
        }
    }

    /// The two-phase rewrite's driver: hand tee generations to the
    /// worker (append+fsync off-thread) while they CONVERGE — each
    /// generation covers only the ingest that landed during the
    /// previous one's disk write, so with ingest below disk bandwidth
    /// the sizes shrink geometrically toward `SMALL_TEE`, and the
    /// reactor's synchronous cost is one ≤4 MiB append + rename.
    ///
    /// When ingest outruns the disk the generations do NOT shrink; the
    /// old policy force-swapped after 4 handoffs and the reactor paid a
    /// bounded-LARGE synchronous append — median tailgate measured up
    /// to a 6 s client-visible stall on the mixed cell
    /// (the third-seat finding in bench/). Now a non-shrinking generation
    /// (or the hard cap) ABORTS the rewrite instead and re-anchors the
    /// auto-rewrite growth rule at the current size: under sustained
    /// overload the log grows and the server degrades; it does not
    /// stall. The live file has every write via the normal append path
    /// — an abort risks no data, ever.
    #[cold]
    pub(crate) fn advance_rewrite_handoff(&mut self) {
        const SMALL_TEE: usize = 4 << 20; // 4 MiB: ms-scale append+sync
        /// A generation must be at most this fraction (×1/2) of the
        /// previous one to count as converging.
        const SHRINK_NUM: usize = 1;
        const SHRINK_DEN: usize = 2;
        /// Hard cap even while shrinking — a backstop, not the policy.
        const MAX_HANDOFFS: u8 = 12;
        let Some(h) = self.rewrite_handoff.take() else {
            return;
        };
        let Some(aof) = &mut self.aof else { return };
        if aof.tee_watermarks().is_some() {
            self.advance_filetee(h);
            return;
        }
        let tee = aof.take_tee_for_handoff().unwrap_or_default();
        if tee.len() <= SMALL_TEE {
            self.finish_rewrite_swap(&h, tee);
            return;
        }
        let shrinking = tee.len() <= h.prev_len / SHRINK_DEN * SHRINK_NUM;
        if h.iters >= MAX_HANDOFFS || (h.iters > 0 && !shrinking) || tee.len() > TEE_DEFER_CAP {
            eprintln!(
                "kevy: shard {} aof rewrite deferred: tee generation {} B after {} handoffs \
                 (ingest outrunning disk) — auto-rewrite re-anchored at current size",
                self.id,
                tee.len(),
                h.iters
            );
            aof.anchor_rewrite_deferred();
            self.abort_rewrite_cleanup(&h.tmp);
            return;
        }
        self.hand_off_generation(h, tee);
    }

    /// File-tee advance: fold `[consumed, handed)` while the lag
    /// halves per step, finish when the whole remainder (staging
    /// included) fits the synchronous bound, defer on divergence or
    /// overrun — the same policy as the Vec tee, expressed in file
    /// offsets. Runs under the structural gate (ring drained), so
    /// `handed` bytes are all in the file.
    fn advance_filetee(&mut self, h: RewriteHandoff) {
        const SMALL_TEE: u64 = 4 << 20;
        const MAX_HANDOFFS: u8 = 12;
        let Some(aof) = &mut self.aof else { return };
        let Some((consumed, handed)) = aof.tee_watermarks() else { return };
        let lag = aof.tee_len().unwrap_or(0) as u64; // incl. staging
        if lag <= SMALL_TEE {
            self.finish_filetee_swap(&h);
            return;
        }
        let shrinking = handed - consumed <= (h.prev_len as u64) / 2;
        if h.iters >= MAX_HANDOFFS
            || (h.iters > 0 && !shrinking)
            || lag > TEE_DEFER_CAP as u64
        {
            eprintln!(
                "kevy: shard {} aof rewrite deferred: tee lag {lag} B after {} folds \
                 (ingest outrunning disk) — auto-rewrite re-anchored at current size",
                self.id, h.iters
            );
            aof.anchor_rewrite_deferred();
            self.abort_rewrite_cleanup(&h.tmp);
            return;
        }
        self.submit_tee_fold(h, consumed, handed);
    }

    /// Ship one fold `[consumed, handed)` to the worker; a gone worker
    /// or unclonable handle aborts (live AOF carried every write).
    fn submit_tee_fold(&mut self, h: RewriteHandoff, consumed: u64, handed: u64) {
        let src = match self.aof.as_ref().and_then(kevy_persist::Aof::tee_copy_handle) {
            Some(Ok(f)) => f,
            _ => {
                eprintln!("kevy: shard {} tee handle clone failed — rewrite aborted", self.id);
                self.abort_rewrite_cleanup(&h.tmp);
                return;
            }
        };
        let job = PersistJob::TeeCopy {
            src,
            from: consumed,
            to: handed,
            tmp: h.tmp.clone(),
        };
        if self.persist.submit(self.id, job) {
            self.rewrite_handoff = Some(RewriteHandoff {
                iters: h.iters + 1,
                prev_len: (handed - consumed) as usize,
                ..h
            });
        } else {
            eprintln!(
                "kevy: shard {} persist worker unavailable for tee fold — rewrite aborted",
                self.id
            );
            self.abort_rewrite_cleanup(&h.tmp);
        }
    }

    /// The bounded synchronous final swap, file-tee mode: staging
    /// remainder + ≤SMALL_TEE tail fold, rename, ship the `.tee` for
    /// an off-thread unlink.
    fn finish_filetee_swap(&mut self, h: &RewriteHandoff) {
        let Some(aof) = &mut self.aof else { return };
        match aof.finish_concurrent_rewrite_from_tee(&h.tmp, h.keys) {
            Ok((_stats, tee_path)) => self.ship_cleanup(vec![tee_path], Vec::new()),
            Err(e) => {
                eprintln!("kevy: shard {} aof rewrite swap failed: {e}", self.id);
                self.abort_rewrite_cleanup(&h.tmp);
            }
        }
    }

    /// A tee generation landed (or failed) on the worker: recycle its
    /// cleared buffer into the pool either way, then advance the
    /// handoff — or tear the rewrite down on an append error.
    fn on_tee_appended(&mut self, result: std::io::Result<()>, tmp: &std::path::Path, buf: Vec<u8>) {
        if let Some(aof) = &mut self.aof {
            aof.stash_tee_spare(buf);
        }
        match result {
            Ok(()) => self.advance_rewrite_handoff(),
            Err(e) => {
                eprintln!("kevy: shard {} aof rewrite tee append failed: {e}", self.id);
                self.rewrite_handoff = None;
                self.abort_rewrite_cleanup(tmp);
            }
        }
    }

    /// Tick-side overrun check: a rewrite whose tee has outgrown
    /// [`TEE_DEFER_CAP`] while the image is still dumping (or between
    /// handoffs) is deferred NOW — the growth itself is the damage.
    pub(crate) fn check_tee_overrun(&mut self) {
        let Some(aof) = &mut self.aof else { return };
        let Some(len) = aof.tee_len() else { return };
        if len <= TEE_DEFER_CAP {
            return;
        }
        eprintln!(
            "kevy: shard {} aof rewrite deferred mid-flight: tee at {len} B \
             (ingest outrunning disk) — auto-rewrite re-anchored at current size",
            self.id
        );
        aof.anchor_rewrite_deferred();
        let tmp = self.rewrite_handoff.take().map(|h| h.tmp);
        match tmp {
            // Between handoffs: the tmp image is ours to delete.
            Some(t) => self.abort_rewrite_cleanup(&t),
            // Image still dumping: the completion arm sees the abort
            // (is_rewriting false) and cleans the tmp up itself.
            None => {
                let mut paths = Vec::new();
                let mut bufs = Vec::new();
                if let Some(aof) = &mut self.aof {
                    bufs = aof.take_tee_teardown();
                    if let Some(tee_path) = aof.take_tee_file_teardown() {
                        paths.push(tee_path);
                    }
                    aof.abort_concurrent_rewrite();
                }
                self.ship_cleanup(paths, bufs);
            }
        }
    }

    /// Best-effort teardown unlinks that failed — named for the log.
    fn note_cleanup_failures(&self, failed: Vec<(std::path::PathBuf, std::io::Error)>) {
        for (path, e) in failed {
            eprintln!(
                "kevy: shard {} teardown file {} not deleted: {e}",
                self.id,
                path.display()
            );
        }
    }

    /// A tee-file fold landed (or failed): advance the consumed
    /// watermark and take the next step, or tear the rewrite down.
    fn on_tee_copied(&mut self, result: std::io::Result<()>, tmp: std::path::PathBuf, to: u64) {
        match result {
            Ok(()) => {
                if let Some(aof) = &mut self.aof {
                    aof.tee_advance_consumed(to);
                }
                self.advance_rewrite_handoff();
            }
            Err(e) => {
                eprintln!("kevy: shard {} aof tee fold failed: {e}", self.id);
                self.rewrite_handoff = None;
                self.abort_rewrite_cleanup(&tmp);
            }
        }
    }

    /// Ship one (still-shrinking) generation to the worker. A gone
    /// worker aborts: the handed-off tee is lost with it, so the tmp
    /// image is incomplete — and the live file carried every write.
    fn hand_off_generation(&mut self, h: RewriteHandoff, tee: Vec<u8>) {
        let prev_len = tee.len();
        let job = PersistJob::TeeAppend {
            tmp: h.tmp.clone(),
            bytes: tee,
        };
        if self.persist.submit(self.id, job) {
            self.rewrite_handoff = Some(RewriteHandoff {
                iters: h.iters + 1,
                prev_len,
                ..h
            });
        } else {
            eprintln!(
                "kevy: shard {} persist worker unavailable for tee handoff — rewrite aborted",
                self.id
            );
            self.abort_rewrite_cleanup(&h.tmp);
        }
    }

    /// The bounded synchronous final swap (`tee` ≤ `SMALL_TEE`):
    /// append + fsync the last generation, rename, reopen.
    fn finish_rewrite_swap(&mut self, h: &RewriteHandoff, tee: Vec<u8>) {
        let Some(aof) = &mut self.aof else { return };
        if let Err(e) = aof.finish_concurrent_rewrite_with(&h.tmp, h.keys, tee) {
            eprintln!("kevy: shard {} aof rewrite swap failed: {e}", self.id);
            self.abort_rewrite_cleanup(&h.tmp);
            return;
        }
        // The recycled spare can still hold a GB-scale warm buffer —
        // its free belongs on the worker.
        let bufs = self.aof.as_mut().map(kevy_persist::Aof::take_tee_teardown).unwrap_or_default();
        self.ship_cleanup(Vec::new(), bufs);
    }


    /// Common abort tail: drop every retained tee resource and the
    /// half-built image via ONE worker Cleanup job (the worker is
    /// serial; split submits silently dropped the second — measured as
    /// inline GB unlinks sneaking back onto the reactor). The live AOF
    /// carried every write through the normal append path, so an abort
    /// never risks data. Worker gone = inline best-effort (shutdown or
    /// error path; nothing latency-critical left to protect).
    fn abort_rewrite_cleanup(&mut self, tmp: &std::path::Path) {
        let mut paths = vec![tmp.to_path_buf()];
        let mut bufs = Vec::new();
        if let Some(aof) = &mut self.aof {
            bufs = aof.take_tee_teardown();
            if let Some(tee_path) = aof.take_tee_file_teardown() {
                paths.push(tee_path);
            }
            aof.abort_concurrent_rewrite();
        }
        self.ship_cleanup(paths, bufs);
    }

    /// One Cleanup submit; inline fallback when the worker is gone.
    fn ship_cleanup(&mut self, paths: Vec<std::path::PathBuf>, bufs: Vec<Vec<u8>>) {
        if paths.is_empty() && bufs.is_empty() {
            return;
        }
        let job = PersistJob::Cleanup {
            paths: paths.clone(),
            bufs,
        };
        if !self.persist.submit(self.id, job) {
            for p in paths {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}
