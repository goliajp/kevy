//! S3: the epoll/kqueue reactors' AOF writer lane — the poll-based
//! counterpart of the io_uring offload (`uring_aof`). Appends queue on
//! [`kevy_persist::Aof`] exactly as under S1; a per-shard thread drains
//! them with sequential `write_all`s on a cloned O_APPEND handle
//! (byte-identical to the synchronous path) and runs the fsyncs, so
//! the reactor never blocks on either. Fsync completions carry the S2
//! record watermark: under `always` the reply gate in `flush_conn`
//! holds output until the covering fsync lands, and the lane wakes the
//! shard through its poller waker to release it.
//!
//! Ordering: one mpsc channel is the whole discipline — appends and
//! fsyncs execute in submission order, so a fsync submitted after the
//! queue drained provably covers every earlier record (the same
//! drain-then-fsync contract the ring path keeps via SQ emptiness).

use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Instant;

use crate::Commands;
use crate::shard::Shard;
use kevy_sys::Waker;

enum Job {
    Append(Vec<u8>),
    Fsync {
        covers: u64,
    },
    /// Swap the lane onto a fresh handle (post-rewrite reopen). The
    /// old clone points at the renamed-away inode; dropping it here
    /// keeps that close off the reactor.
    Reopen(File),
}

enum Done {
    Appended,
    Synced { covers: u64 },
    Failed { what: &'static str, err: String },
}

/// Per-shard lane state. Lives on [`Shard`]; every method is a no-op
/// unless [`Self::enabled`].
pub(crate) struct AofWriterLane {
    pub(crate) enabled: bool,
    chans: Option<(Sender<Job>, Receiver<Done>)>,
    /// Append jobs submitted vs completed — the structural-op drain
    /// predicate (a rewrite swap must not run over in-flight writes).
    submitted: u64,
    completed: u64,
    fsync_inflight: bool,
    /// An append completed since the last fsync completion.
    dirty_since_sync: bool,
    last_sync: Instant,
    /// Records fsync-proven durable (S2 semantics — compare against
    /// [`kevy_persist::Aof::queued_watermark`] stamps).
    pub(crate) durable_watermark: u64,
    /// Conns whose output the always gate held; flushed on watermark
    /// advance (the poller would otherwise spin on a writable socket
    /// we refuse to write).
    pub(crate) held_conns: Vec<u64>,
    /// A structural op arrived while writes were in flight; the tick
    /// retries it after the drain.
    pub(crate) want_restructure: bool,
}

impl Default for AofWriterLane {
    fn default() -> Self {
        Self {
            enabled: false,
            chans: None,
            submitted: 0,
            completed: 0,
            fsync_inflight: false,
            dirty_since_sync: false,
            last_sync: Instant::now(),
            durable_watermark: 0,
            held_conns: Vec::new(),
            want_restructure: false,
        }
    }
}

impl AofWriterLane {
    /// Spawn the worker on `file` (a `queued_file_clone`). The thread
    /// exits when the job sender drops (shard teardown).
    fn spawn(&mut self, shard_id: usize, file: File, waker: Arc<Waker>) {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Done>();
        let spawned = std::thread::Builder::new()
            .name(format!("kevy-aofw-{shard_id}"))
            .spawn(move || run_worker(file, &job_rx, &done_tx, &waker));
        if spawned.is_ok() {
            self.chans = Some((job_tx, done_rx));
            self.enabled = true;
        } else {
            eprintln!(
                "kevy: shard {shard_id} aof writer thread failed to spawn; keeping the synchronous path"
            );
        }
    }

    fn submit(&mut self, job: Job) -> Result<(), Job> {
        match &self.chans {
            Some((tx, _)) => tx.send(job).map_err(|e| e.0),
            None => Err(job),
        }
    }

    /// Every append this lane has taken is completed and nothing else
    /// is queued behind them.
    fn appends_drained(&self) -> bool {
        self.submitted == self.completed
    }
}

/// Worker body: strict submission order; a failed write retries in
/// place (loud, 10 ms backoff) because the bytes exist only here — the
/// same never-drop contract the ring path keeps by re-arming the SQE.
fn run_worker(mut file: File, jobs: &Receiver<Job>, done: &Sender<Done>, waker: &Arc<Waker>) {
    while let Ok(job) = jobs.recv() {
        let d = match job {
            Job::Append(bytes) => {
                while let Err(e) = file.write_all(&bytes) {
                    eprintln!("kevy: aof writer append failed: {e}; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Done::Appended
            }
            Job::Fsync { covers } => match file.sync_data() {
                Ok(()) => Done::Synced { covers },
                Err(e) => Done::Failed { what: "fsync", err: e.to_string() },
            },
            Job::Reopen(f) => {
                file = f;
                continue;
            }
        };
        if done.send(d).is_err() {
            return; // shard gone
        }
        // Best-effort: a failed wake only delays the reap to the next
        // natural poller wakeup.
        let _ = waker.wake();
    }
}

impl<C: Commands> Shard<C> {
    /// Writer-lane mode at reactor setup — DEFAULT ON for the
    /// epoll/kqueue reactor with an AOF (mirrors `uring_aof_setup`;
    /// `KEVY_AOF_OFFLOAD=0/off/no/false` keeps the classic synchronous
    /// path). Under `always` the replies are watermark-gated in
    /// `flush_conn` instead of fsync-blocking the reactor.
    pub(crate) fn epoll_aof_setup(&mut self) {
        let Some(aof) = &mut self.aof else { return };
        if matches!(std::env::var("KEVY_AOF_OFFLOAD").as_deref(), Ok("0" | "off" | "no" | "false"))
        {
            return;
        }
        aof.enable_queued_appends();
        let file = match aof.queued_file_clone() {
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                eprintln!(
                    "kevy: shard {} aof handle clone failed: {e}; keeping the synchronous path",
                    self.id
                );
                return;
            }
            None => return,
        };
        let (id, waker) = (self.id, Arc::clone(&self.waker));
        self.aof_lane.spawn(id, file, waker);
    }

    /// Per-iteration pump (replaces the synchronous `maybe_sync` call
    /// in the reactor loop): reap completions, hand new queue contents
    /// to the worker, schedule the fsync. Falls back to the classic
    /// `maybe_sync` when the lane is off — byte-identical behavior.
    pub(crate) fn epoll_aof_tick(&mut self) {
        if !self.aof_lane.enabled {
            if let Some(aof) = &mut self.aof {
                let _ = aof.maybe_sync();
            }
            return;
        }
        self.epoll_aof_reap();
        let holding = self.aof.as_ref().is_some_and(kevy_persist::Aof::swap_holding);
        if !holding
            && let Some(aof) = &mut self.aof
            && let Some((_, bytes)) = aof.take_pending()
        {
            match self.aof_lane.submit(Job::Append(bytes)) {
                Ok(()) => self.aof_lane.submitted += 1,
                Err(Job::Append(bytes)) => {
                    // The chunk's bytes exist only here — put them
                    // back at the queue front so the fallback's
                    // flush_queued lands them exactly once, in order.
                    if let Some(aof) = &mut self.aof {
                        aof.requeue_front(bytes);
                    }
                    self.epoll_aof_dead_lane();
                }
                Err(_) => self.epoll_aof_dead_lane(),
            }
        }
        if !holding {
            self.epoll_aof_maybe_fsync();
        }
    }

    /// Drain the done channel: advance the durable watermark on fsync
    /// completions and release anything the always gate held.
    fn epoll_aof_reap(&mut self) {
        let mut advanced = false;
        loop {
            let msg = match &self.aof_lane.chans {
                Some((_, rx)) => rx.try_recv(),
                None => return,
            };
            match msg {
                Ok(Done::Appended) => {
                    self.aof_lane.completed += 1;
                    self.aof_lane.dirty_since_sync = true;
                }
                Ok(Done::Synced { covers }) => {
                    self.aof_lane.fsync_inflight = false;
                    self.aof_lane.dirty_since_sync = false;
                    self.aof_lane.last_sync = Instant::now();
                    self.aof_lane.durable_watermark = self.aof_lane.durable_watermark.max(covers);
                    advanced = true;
                }
                Ok(Done::Failed { what, err }) => {
                    // Same contract as the ring path: the watermark
                    // stays put (held replies stay held — no false
                    // ack) and the tick resubmits.
                    self.aof_lane.fsync_inflight = false;
                    eprintln!("kevy: shard {} aof writer {what} failed: {err}", self.id);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.epoll_aof_dead_lane();
                    return;
                }
            }
        }
        if advanced {
            self.epoll_release_held();
        }
    }

    /// One DATASYNC at a time, only when the queue and the worker are
    /// drained of appends (submission order then makes it cover every
    /// queued record). always: as soon as any record is unproven.
    /// everysec: on the 1 s cadence.
    fn epoll_aof_maybe_fsync(&mut self) {
        let lane = &self.aof_lane;
        if lane.fsync_inflight || !lane.appends_drained() {
            return;
        }
        let Some(aof) = &self.aof else { return };
        if !aof.queued_is_empty() {
            return;
        }
        let due = match aof.fsync_policy() {
            kevy_persist::Fsync::Always => aof.queued_watermark() > lane.durable_watermark,
            kevy_persist::Fsync::EverySec => {
                lane.dirty_since_sync && lane.last_sync.elapsed().as_secs() >= 1
            }
            kevy_persist::Fsync::No => false,
        };
        if !due {
            return;
        }
        let covers = aof.queued_watermark();
        if self.aof_lane.submit(Job::Fsync { covers }).is_ok() {
            self.aof_lane.fsync_inflight = true;
        } else {
            self.epoll_aof_dead_lane();
        }
    }

    /// Flush every conn the always gate parked, plus any held
    /// cross-shard response batches the watermark now covers.
    fn epoll_release_held(&mut self) {
        let held = std::mem::take(&mut self.aof_lane.held_conns);
        for cid in held {
            let _ = self.flush_conn(cid);
        }
        self.flush_held_responses();
    }

    /// The worker is gone (panicked/exited): fall back to the honest
    /// synchronous path for good. Queued bytes are safe — they are
    /// still in the queue or were written before the death; the
    /// structural entry points' `flush_queued` covers the rest.
    #[cold]
    fn epoll_aof_dead_lane(&mut self) {
        if self.aof_lane.enabled {
            eprintln!(
                "kevy: shard {} aof writer lane lost; reverting to the synchronous path",
                self.id
            );
        }
        self.aof_lane.enabled = false;
        self.aof_lane.chans = None;
        // Anything held must not wait on a fsync that will never come:
        // sync in place, mark durable, release.
        if let Some(aof) = &mut self.aof {
            if let Err(e) = aof.sync_now() {
                eprintln!("kevy: shard {} fallback sync failed: {e}", self.id);
                return; // keep holds — no false ack
            }
            self.aof_lane.durable_watermark = aof.queued_watermark();
        }
        self.epoll_release_held();
    }

    /// S3 stamp (the epoll counterpart of `uring_stamp_hold`): if the
    /// dispatch bracketed by `w0` appended queued records, mark the
    /// conn's pending output as held until the covering fsync. A conn
    /// holding replies for several batches keeps the highest stamp.
    pub(crate) fn epoll_stamp_hold(&mut self, w0: Option<u64>, cid: u64) {
        let Some(w0) = w0 else { return };
        if !self.aof_lane.enabled {
            return; // uring shards stamp through their own path
        }
        let Some(aof) = self.aof.as_ref() else { return };
        let w1 = aof.queued_watermark();
        if w1 > w0
            && let Some(c) = self.conns.get_mut(&cid)
        {
            c.held_watermark = Some(c.held_watermark.map_or(w1, |h| h.max(w1)));
        }
    }

    /// May a structural file operation run right now? (The epoll
    /// counterpart of `uring_aof_restructure_ready`.)
    pub(crate) fn epoll_aof_restructure_ready(&self) -> bool {
        !self.aof_lane.enabled
            || (self.aof_lane.appends_drained()
                && self.aof.as_ref().is_none_or(kevy_persist::Aof::queued_is_empty))
    }

    /// Tick wrapper for the persistence trio (the epoll counterpart of
    /// `uring_tick_persist`): structural work only runs with the lane
    /// drained; a deferred BGREWRITEAOF fires here.
    pub(crate) fn epoll_tick_persist(&mut self) {
        self.check_tee_overrun();
        if self.epoll_aof_restructure_ready() {
            if self.aof_lane.want_restructure {
                self.aof_lane.want_restructure = false;
                self.start_bg_rewrite();
            }
            self.tick_persist();
        }
    }

    /// A structural op (worker-fsynced image swap) made everything
    /// queued so far durable; also hand the lane the post-swap handle.
    pub(crate) fn epoll_aof_on_swap_finalized(&mut self) {
        if !self.aof_lane.enabled {
            return;
        }
        if let Some(aof) = &self.aof {
            self.aof_lane.durable_watermark =
                self.aof_lane.durable_watermark.max(aof.queued_watermark());
            match aof.queued_file_clone() {
                Some(Ok(f)) => {
                    if self.aof_lane.submit(Job::Reopen(f)).is_err() {
                        self.epoll_aof_dead_lane();
                        return;
                    }
                }
                Some(Err(e)) => {
                    eprintln!("kevy: shard {} post-swap handle clone failed: {e}", self.id);
                    self.epoll_aof_dead_lane();
                    return;
                }
                None => {}
            }
        }
        self.epoll_release_held();
    }

    /// Settle the lane: wait for in-flight appends to complete. Used
    /// at reactor exit (so the shutdown `sync_now` sees a complete
    /// file) and before any owner-handle write that would otherwise
    /// interleave with the clone's (fsync-policy switch).
    pub(crate) fn epoll_aof_settle(&mut self) {
        while self.aof_lane.enabled && !self.aof_lane.appends_drained() {
            self.epoll_aof_reap();
            std::thread::yield_now();
        }
    }
}
