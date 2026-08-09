//! The two-phase AOF rewrite's completion half on [`Shard`]: applying
//! the worker's `Rewrite` / `TeeAppend` results and driving the tee
//! handoff loop. Split from `persist_worker.rs` at the 500-LOC line —
//! the Aof-side state transitions live in kevy-persist's
//! `aof_rewrite.rs`; this file owns the reactor-side protocol.

use crate::Commands;
use crate::persist_worker::{PersistDone, PersistJob};
use crate::shard::Shard;

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
            PersistDone::DropBufs => {}
            PersistDone::Remove { result, path } => self.note_remove_done(result, &path),
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
        let tee = aof.take_tee_for_handoff().unwrap_or_default();
        if tee.len() <= SMALL_TEE {
            self.finish_rewrite_swap(&h, tee);
            return;
        }
        let shrinking = tee.len() <= h.prev_len / SHRINK_DEN * SHRINK_NUM;
        if h.iters >= MAX_HANDOFFS || (h.iters > 0 && !shrinking) {
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

    /// Off-thread unlink completed — best-effort, log-only on failure
    /// (an orphaned `.rewrite` tmp is reclaimed by the next rewrite's
    /// truncating open of the same deterministic path).
    fn note_remove_done(&self, result: std::io::Result<()>, path: &std::path::Path) {
        if let Err(e) = result {
            eprintln!(
                "kevy: shard {} abandoned rewrite image {} not deleted: {e}",
                self.id,
                path.display()
            );
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
        // its free belongs on the worker with the rest of them.
        self.ship_tee_teardown();
    }

    /// Ship every retained tee buffer to the worker for an off-thread
    /// drop. Worker gone/busy = drop inline (shutdown or error path —
    /// nothing latency-critical left to protect).
    fn ship_tee_teardown(&mut self) {
        let Some(aof) = &mut self.aof else { return };
        let bufs = aof.take_tee_teardown();
        if bufs.is_empty() {
            return;
        }
        if !self.persist.submit(self.id, PersistJob::DropBufs { bufs }) {
            // submit dropped the job (and the buffers) on the floor
            // already — nothing further to do.
        }
    }

    /// Common abort tail: drop the tee, delete the half-built image.
    /// The live AOF carried every write through the normal append path,
    /// so an abort never risks data. The unlink goes to the worker —
    /// deleting a multi-GB image contends on the fs journal (seconds
    /// under a saturated disk) and must not run on the reactor; the
    /// worker runs jobs serially, so a later Rewrite can never race the
    /// pending Remove on the same deterministic tmp path. Worker gone =
    /// inline best-effort (shutdown path; nothing left to stall).
    fn abort_rewrite_cleanup(&mut self, tmp: &std::path::Path) {
        self.ship_tee_teardown();
        if let Some(aof) = &mut self.aof {
            aof.abort_concurrent_rewrite();
        }
        if !self.persist.submit(
            self.id,
            PersistJob::Remove {
                path: tmp.to_path_buf(),
            },
        ) {
            let _ = std::fs::remove_file(tmp);
        }
    }
}
