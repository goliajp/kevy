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
            PersistDone::SwapImage { result, trash } => self.on_swap_done(result, trash),
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
        if tee.is_empty() || (tee.len() <= SMALL_TEE && h.iters >= MAX_HANDOFFS) {
            // Terminal: converged (empty) or the trickle backstop —
            // either way the residual is ≤ SMALL_TEE and rides the
            // swap (off-thread in queued mode; see finish_terminal).
            self.finish_terminal(h, tee);
            return;
        }
        let shrinking = tee.len() <= h.prev_len / SHRINK_DEN * SHRINK_NUM;
        let small = tee.len() <= SMALL_TEE;
        if (h.iters >= MAX_HANDOFFS && !small)
            || (h.iters > 0 && !shrinking && !small)
            || tee.len() > TEE_DEFER_CAP
        {
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


    /// Terminal step: the residual tee (possibly empty) is small enough
    /// to ride the swap. Queued mode hands it to the worker as the
    /// image's tail — append, fsync, hardlink, rename, all off-thread;
    /// the buffer (which can carry a large CAPACITY even at len 0 —
    /// clears never shrink) is freed on the worker too. Non-queued
    /// (epoll) keeps the classic synchronous swap: appends write
    /// straight to the live fd — nothing can hold them through a
    /// worker-side rename (they would land on the renamed-away inode
    /// and vanish).
    fn finish_terminal(&mut self, h: RewriteHandoff, tee: Vec<u8>) {
        let queued = self.aof.as_ref().is_some_and(kevy_persist::Aof::queued_mode);
        if queued {
            self.submit_offthread_swap(h, tee);
        } else {
            self.finish_rewrite_swap(&h, tee);
        }
    }

    /// A tee generation landed (or failed) on the worker: recycle its
    /// cleared buffer into the pool either way, then advance the
    /// handoff — or tear the rewrite down on an append error.
    fn on_tee_appended(&mut self, result: std::io::Result<()>, tmp: &std::path::Path, buf: Vec<u8>) {
        if let Some(aof) = &mut self.aof {
            // The spare slot's loser can still carry GB capacity —
            // ship it to the worker like every other big free (tiny
            // capacities drop inline; the channel hop would cost more).
            if let Some(evicted) = aof.stash_tee_spare(buf)
                && evicted.capacity() >= 1 << 20
            {
                self.ship_cleanup(Vec::new(), vec![evicted]);
            }
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
                let bufs = match &mut self.aof {
                    Some(aof) => {
                        let b = aof.take_tee_teardown();
                        aof.abort_concurrent_rewrite();
                        b
                    }
                    None => Vec::new(),
                };
                self.ship_cleanup(Vec::new(), bufs);
            }
        }
    }

    /// Off-thread swap submit: even rename+hardlink are journal work
    /// that blocks ~400ms behind a loaded jbd2 commit (the tick
    /// sub-probe named poll=416ms x4 shards in one window). The worker
    /// does them; the reactor holds its queue (appends accumulate,
    /// bounded by the hold) and reopens on Done. Worker gone
    /// (shutdown) falls back to the synchronous swap.
    fn submit_offthread_swap(&mut self, h: RewriteHandoff, tail: Vec<u8>) {
        let Some(aof) = &mut self.aof else { return };
        let live = aof.live_path();
        let trash = aof.swap_trash_name();
        aof.begin_swap_hold();
        let job = PersistJob::SwapImage {
            tmp: h.tmp.clone(),
            live,
            trash,
            tail,
        };
        match self.persist.submit_reclaim_tail(self.id, job) {
            Ok(()) => self.rewrite_handoff = Some(h), // keys carried to finalize
            Err(tail) => {
                // Worker gone: the reclaimed tail's bytes exist only in
                // that buffer — synchronous fallback carries them.
                if let Some(aof) = &mut self.aof {
                    aof.abort_swap_hold();
                }
                self.finish_rewrite_swap(&h, tail);
            }
        }
    }

    /// The worker's rename landed (or failed): reactor-side finalize
    /// (reopen + anchors, µs) or abort (live path still the old log).
    fn on_swap_done(&mut self, result: std::io::Result<()>, trash: Option<std::path::PathBuf>) {
        let Some(h) = self.rewrite_handoff.take() else { return };
        let Some(aof) = &mut self.aof else { return };
        match result {
            Ok(()) => match aof.swap_finalize_reopen(h.keys, trash) {
                Ok(_stats) => {
                    // The worker sync_all'd the image and journaled the
                    // rename: everything queued before the swap is
                    // durable — release any Always-held replies.
                    #[cfg(target_os = "linux")]
                    self.uring_aof_mark_all_durable();
                    self.epoll_aof_on_swap_finalized();
                    let paths = self
                        .aof
                        .as_mut()
                        .and_then(kevy_persist::Aof::take_swap_trash)
                        .into_iter()
                        .collect();
                    let bufs = self
                        .aof
                        .as_mut()
                        .map(kevy_persist::Aof::take_tee_teardown)
                        .unwrap_or_default();
                    self.ship_cleanup(paths, bufs);
                }
                Err(e) => {
                    // Rename landed but reopen failed — the log IS the
                    // new image; keep trying to reopen is the only
                    // honest move, but at minimum say it loudly.
                    eprintln!(
                        "kevy: shard {} post-swap reopen failed: {e} — appends will error until reopened",
                        self.id
                    );
                }
            },
            Err(e) => {
                eprintln!("kevy: shard {} off-thread swap failed: {e}", self.id);
                aof.abort_swap_hold();
                self.abort_rewrite_cleanup(&h.tmp);
            }
        }
    }

    /// Best-effort teardown unlinks that failed — named for the log
    /// (an orphaned `.rewrite` tmp is reclaimed by the next rewrite's
    /// truncating open of the same deterministic path).
    fn note_cleanup_failures(&self, failed: Vec<(std::path::PathBuf, std::io::Error)>) {
        for (path, e) in failed {
            eprintln!(
                "kevy: shard {} teardown file {} not deleted: {e}",
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
        let spent = match aof.finish_concurrent_rewrite_with(&h.tmp, h.keys, tee) {
            Ok((_stats, spent)) => spent,
            Err(e) => {
                eprintln!("kevy: shard {} aof rewrite swap failed: {e}", self.id);
                self.abort_rewrite_cleanup(&h.tmp);
                return;
            }
        };
        // Ship the pre-swap log's graveyard link, the spent tee
        // buffers, and any GB-scale warm spare to the worker — all
        // these frees contend the journal/LRU.
        let (paths, mut bufs) = match &mut self.aof {
            Some(aof) => (
                aof.take_swap_trash().into_iter().collect::<Vec<_>>(),
                aof.take_tee_teardown(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        bufs.extend(spent);
        self.ship_cleanup(paths, bufs);
    }


    /// Common abort tail: retained tee buffers and the half-built
    /// image go to the worker in ONE Cleanup job (the worker is
    /// serial; split submits silently dropped the second, sneaking GB
    /// unlinks back inline onto the reactor). The live AOF carried
    /// every write through the normal append path, so an abort never
    /// risks data. Worker gone = inline best-effort (shutdown/error
    /// path; nothing latency-critical left to protect).
    fn abort_rewrite_cleanup(&mut self, tmp: &std::path::Path) {
        let bufs = match &mut self.aof {
            Some(aof) => {
                let b = aof.take_tee_teardown();
                aof.abort_concurrent_rewrite();
                b
            }
            None => Vec::new(),
        };
        self.ship_cleanup(vec![tmp.to_path_buf()], bufs);
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
