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
        if self.persist.busy() || self.aof.as_ref().is_some_and(|a| a.is_rewriting()) {
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
        if self.persist.busy() || self.aof.as_ref().is_none_or(|a| a.is_rewriting()) {
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
        match done {
            PersistDone::Save { result: Ok(tmp), snap_path, aof_reset } => {
                if let Err(e) = std::fs::rename(&tmp, &snap_path) {
                    eprintln!("kevy: shard {} bgsave rename failed: {e}", self.id);
                    self.abort_persist_tee(aof_reset);
                    return;
                }
                if let (Some(reset_tmp), Some(aof)) = (aof_reset, &mut self.aof) {
                    // The reset log's base is just the magic header; the
                    // tee'd post-collect writes are appended by finish.
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

    fn abort_persist_tee(&mut self, aof_reset: Option<PathBuf>) {
        if aof_reset.is_some()
            && let Some(aof) = &mut self.aof
        {
            aof.abort_concurrent_rewrite();
        }
    }
}
