//! Per-shard background persistence — the serialize-and-spill half of COW
//! snapshots/rewrites (`BGSAVE`, `BGREWRITEAOF`, the tick auto-rewrite).
//!
//! The shard thread freezes a [`SnapshotView`] (O(n)-shallow, see
//! `kevy_store::Store::collect_snapshot`) and hands it to a lazily-spawned
//! worker thread that does the actual serialization + disk I/O. Completions
//! come back over a channel and are applied from the shard's tick:
//!
//! - **BgSave**: the worker writes the snapshot's durable `<path>.tmp`
//!   (no rename) and the tick renames it *and* swaps in the tee'd AOF
//!   reset in one adjacent critical section — the snapshot/AOF commit
//!   stays microseconds apart (same exposure as the synchronous `SAVE`),
//!   not the seconds the background write takes.
//! - **RewriteAof**: the worker dumps the view as RESP commands to the
//!   `.rewrite` temp; the tick appends the tee'd diff and swaps
//!   (`Aof::finish_concurrent_rewrite`).
//!
//! One job in flight per shard (the Redis single-bgsave discipline); a
//! request landing while busy is skipped with a log line. A failed job
//! aborts the tee — the live AOF and the previous snapshot are untouched.

use crate::Commands;
use crate::shard::Shard;
use kevy_store::SnapshotView;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;

pub(crate) enum PersistJob {
    /// Write `view` to `snap_path`'s durable tmp. `aof_reset` = an AOF tee
    /// was started; the completion swaps in a fresh log of post-collect
    /// writes (the COW replacement for the old save-then-truncate).
    Save {
        view: SnapshotView,
        snap_path: PathBuf,
        aof_reset: Option<PathBuf>,
    },
    /// Dump `view` as RESP commands at the AOF's `.rewrite` tmp.
    Rewrite { view: SnapshotView, tmp: PathBuf },
}

pub(crate) enum PersistDone {
    Save {
        result: io::Result<PathBuf>, // the written snapshot tmp
        snap_path: PathBuf,
        aof_reset: Option<PathBuf>,
    },
    Rewrite {
        result: io::Result<u64>, // keys dumped
        tmp: PathBuf,
    },
}

/// Lazily-spawned single-thread persister. Dropping it closes the channel;
/// the worker exits after finishing any in-flight job.
pub(crate) struct PersistWorker {
    chans: Option<(mpsc::Sender<PersistJob>, mpsc::Receiver<PersistDone>)>,
    in_flight: bool,
}

impl PersistWorker {
    pub(crate) fn new() -> Self {
        Self { chans: None, in_flight: false }
    }

    /// One job in flight at a time — callers check before collecting a view.
    #[inline]
    pub(crate) fn busy(&self) -> bool {
        self.in_flight
    }

    /// Hand a job to the worker (spawning it on first use). Returns `false`
    /// (without panicking) if the worker thread died — callers log + abort.
    pub(crate) fn submit(&mut self, shard_id: usize, job: PersistJob) -> bool {
        let (tx, _) = self.chans.get_or_insert_with(|| {
            let (tx, job_rx) = mpsc::channel::<PersistJob>();
            let (done_tx, done_rx) = mpsc::channel::<PersistDone>();
            std::thread::Builder::new()
                .name(format!("kevy-persist-{shard_id}"))
                .spawn(move || {
                    while let Ok(job) = job_rx.recv() {
                        let done = run_job(job);
                        if done_tx.send(done).is_err() {
                            return; // shard gone — nothing to report to
                        }
                    }
                })
                .expect("spawn persist worker");
            (tx, done_rx)
        });
        if tx.send(job).is_err() {
            return false;
        }
        self.in_flight = true;
        true
    }

    /// Non-blocking completion poll (called from the shard tick).
    pub(crate) fn try_complete(&mut self) -> Option<PersistDone> {
        let (_, rx) = self.chans.as_ref()?;
        match rx.try_recv() {
            Ok(done) => {
                self.in_flight = false;
                Some(done)
            }
            Err(_) => None,
        }
    }

    /// Blocking completion wait — for the shutdown drain only. Returns
    /// `None` when no worker thread was ever spawned (no Save / Rewrite
    /// ever submitted) so callers can short-circuit. Used by
    /// [`Shard::drain_persist_on_shutdown`] after `stop=true`: the
    /// migrated synchronous `SAVE` (formerly inline `save_snapshot`,
    /// now `start_bg_save`) returns `+OK` to the client as soon as the
    /// COW [`SnapshotView`] is frozen, but the on-disk rename to
    /// `dump-{i}.rdb` is gated on `poll_persist_done` in the next
    /// tick. Without a shutdown drain a `stop=true` between the
    /// `start_bg_save` and the tick would leave the snapshot
    /// `.tmp` orphan + the AOF reset never swapped — breaking the
    /// data-survives-restart invariant a client saw `+OK` for.
    pub(crate) fn wait_complete(&mut self) -> Option<PersistDone> {
        let (_, rx) = self.chans.as_ref()?;
        if !self.in_flight {
            return None;
        }
        match rx.recv() {
            Ok(done) => {
                self.in_flight = false;
                Some(done)
            }
            Err(_) => {
                // Worker thread died with a job in flight — drop the
                // in_flight flag so a subsequent shutdown call doesn't
                // block forever.
                self.in_flight = false;
                None
            }
        }
    }
}

fn run_job(job: PersistJob) -> PersistDone {
    match job {
        PersistJob::Save { view, snap_path, aof_reset } => PersistDone::Save {
            result: kevy_persist::write_snapshot_tmp(&view, &snap_path),
            snap_path,
            aof_reset,
        },
        PersistJob::Rewrite { view, tmp } => PersistDone::Rewrite {
            result: kevy_persist::dump_aof(&tmp, &view).map(|(keys, _bytes)| keys),
            tmp,
        },
    }
}

impl<C: Commands> Shard<C> {
    /// `BGSAVE` on this shard: freeze the view, start the AOF tee (the
    /// post-collect writes become the reset log), hand off. Skipped with a
    /// log line if a background job or rewrite is already in flight.
    #[cold]
    pub(crate) fn start_bg_save(&mut self) {
        if self.persist.busy() || self.aof.as_ref().is_some_and(kevy_persist::Aof::is_rewriting) {
            eprintln!("kevy: shard {} bgsave skipped (persist job in flight)", self.id);
            return;
        }
        // collect + begin_view_rewrite back-to-back on this thread: no
        // append can land between them (the tee atomicity contract).
        let view = self.store.collect_snapshot();
        let aof_reset = match &mut self.aof {
            Some(aof) => match aof.begin_view_rewrite() {
                Ok(tmp) => Some(tmp),
                Err(e) => {
                    // Snapshot still proceeds; the AOF just isn't reset, so
                    // a replay stays correct (snapshot ∪ full log ⊇ state —
                    // the log is replayed over the *older* snapshot only
                    // until the next successful save).
                    eprintln!("kevy: shard {} bgsave aof tee failed: {e}", self.id);
                    None
                }
            },
            None => None,
        };
        let job = PersistJob::Save { view, snap_path: self.snapshot_path(), aof_reset };
        if !self.persist.submit(self.id, job) {
            eprintln!("kevy: shard {} persist worker unavailable", self.id);
            if let Some(aof) = &mut self.aof {
                aof.abort_concurrent_rewrite();
            }
        }
    }

    /// `BGREWRITEAOF` / tick auto-rewrite on this shard: freeze the view,
    /// start the tee, dump off-thread. No-op without an AOF (matches the
    /// old synchronous behavior); skipped if a job is already in flight.
    #[cold]
    pub(crate) fn start_bg_rewrite(&mut self) {
        if self.persist.busy() || self.aof.as_ref().is_none_or(kevy_persist::Aof::is_rewriting) {
            if self.aof.is_some() {
                eprintln!("kevy: shard {} aof rewrite skipped (persist job in flight)", self.id);
            }
            return;
        }
        let view = self.store.collect_snapshot();
        let aof = self.aof.as_mut().expect("checked above");
        let tmp = match aof.begin_view_rewrite() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("kevy: shard {} aof rewrite begin failed: {e}", self.id);
                return;
            }
        };
        if !self.persist.submit(self.id, PersistJob::Rewrite { view, tmp }) {
            eprintln!("kevy: shard {} persist worker unavailable", self.id);
            self.aof.as_mut().expect("checked").abort_concurrent_rewrite();
        }
    }

    /// Apply a finished background job (tick path). Success commits in one
    /// adjacent critical section (snapshot rename + AOF swap); failure
    /// aborts the tee and leaves the previous snapshot + live AOF intact.
    #[cold]
    pub(crate) fn poll_persist_done(&mut self) {
        let Some(done) = self.persist.try_complete() else { return };
        self.commit_persist_done(done);
    }

    fn abort_persist_tee(&mut self, aof_reset: Option<PathBuf>) {
        if aof_reset.is_some()
            && let Some(aof) = &mut self.aof
        {
            aof.abort_concurrent_rewrite();
        }
    }

    /// Shutdown drain: block on any in-flight persist job and commit it
    /// (rename of `<dump>.tmp` over `dump-{i}.rdb`, AOF reset swap).
    /// Called from the end of both reactor `run` loops once
    /// `stop=true`. Idempotent / no-op without an in-flight job, so
    /// the steady-state shutdown cost is one `Option::is_some` + one
    /// `bool` check.
    ///
    /// **Why needed**: `Op::Save` was migrated from inline
    /// `save_snapshot` (which blocked the reactor for the entire
    /// disk write) to [`Self::start_bg_save`] — the COW view is
    /// frozen on this thread (8 ns/entry), the serialize + fsync
    /// runs on the per-shard `PersistWorker`, and `+OK` flies back
    /// to the client as soon as the view is frozen. The on-disk
    /// rename is gated on the next tick's [`Self::poll_persist_done`]
    /// to keep that commit in the same lockstep as the AOF reset.
    /// A `stop=true` between the `start_bg_save` and that tick
    /// would leave the snapshot orphan + the client's `+OK`
    /// dishonored. This drain closes the window.
    pub(crate) fn drain_persist_on_shutdown(&mut self) {
        // The persist worker only handles one job at a time, but a
        // completion could still be sitting in the done channel
        // unpolled from the last tick. Take that first, then block
        // on any genuinely in-flight job.
        loop {
            self.poll_persist_done();
            if !self.persist.busy() {
                break;
            }
            // Block for the worker's done message, then route it
            // through the same `poll_persist_done` commit path so
            // the rename + AOF swap rules stay one code path.
            // `wait_complete` returns the `PersistDone` directly so
            // we don't have to re-implement `poll_persist_done`'s
            // match; feed it back via the same fn by stashing it
            // into the worker's done channel — but the channel is
            // private. Instead: handle the done inline via the same
            // match logic. Keep this in sync with `poll_persist_done`.
            let Some(done) = self.persist.wait_complete() else {
                break;
            };
            self.commit_persist_done(done);
        }
    }

    /// Apply a `PersistDone` (the body of `poll_persist_done` minus
    /// the `try_complete` poll). Factored out so both the tick path
    /// and [`Self::drain_persist_on_shutdown`] commit completions
    /// through one rename + AOF-swap implementation.
    #[cold]
    pub(crate) fn commit_persist_done(&mut self, done: PersistDone) {
        match done {
            PersistDone::Save { result: Ok(tmp), snap_path, aof_reset } => {
                if let Err(e) = std::fs::rename(&tmp, &snap_path) {
                    eprintln!("kevy: shard {} bgsave rename failed: {e}", self.id);
                    self.abort_persist_tee(aof_reset);
                    return;
                }
                if let (Some(reset_tmp), Some(aof)) = (aof_reset, &mut self.aof) {
                    let swap = kevy_persist::write_aof_base(&reset_tmp)
                        .and_then(|()| aof.finish_concurrent_rewrite(&reset_tmp, 0));
                    if let Err(e) = swap {
                        eprintln!("kevy: shard {} bgsave aof reset failed: {e}", self.id);
                        aof.abort_concurrent_rewrite();
                        let _ = std::fs::remove_file(&reset_tmp);
                    }
                }
            }
            PersistDone::Save { result: Err(e), aof_reset, .. } => {
                eprintln!("kevy: shard {} bgsave failed: {e}", self.id);
                self.abort_persist_tee(aof_reset);
            }
            PersistDone::Rewrite { result: Ok(keys), tmp } => {
                if let Some(aof) = &mut self.aof
                    && let Err(e) = aof.finish_concurrent_rewrite(&tmp, keys)
                {
                    eprintln!("kevy: shard {} aof rewrite swap failed: {e}", self.id);
                    aof.abort_concurrent_rewrite();
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            PersistDone::Rewrite { result: Err(e), tmp } => {
                eprintln!("kevy: shard {} aof rewrite failed: {e}", self.id);
                if let Some(aof) = &mut self.aof {
                    aof.abort_concurrent_rewrite();
                }
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}
