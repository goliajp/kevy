//! [`Shard::run`] — the epoll/kqueue readiness reactor loop — plus the
//! small path/routing helpers every reactor variant shares. Same
//! `impl<C: Commands> Shard<C>` as [`crate::shard`] (which owns the
//! struct); split out so that file stays under the 500-LOC house rule.

use crate::Commands;
use crate::park_fence;
use crate::shard::Shard;
use crate::shard_lifecycle::Accepted;
use kevy_persist::{Routing, replay_aof};
use kevy_resp::ArgvView;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Work iterations between tick-clock checks. Shared by both reactors:
/// a saturated loop never parks, so the 256-iter counter was its only
/// tick gate — ~1 s of accumulated busy time between clock reads on the
/// mixed tailgate cell, which both delayed BLOCK/WAIT timeouts and made
/// the tick-gap gauge report busy time as a "stall". Batch SIZE was
/// tried first and refuted (the third-seat finding in bench/: one P16
/// completion carries 16 commands, so batch count ≠ work). Counting
/// WORK ITERATIONS is load-proportional: every 4th working iteration
/// pays one vDSO clock read (~30 ns amortized to <10 ns/iter on the
/// -c1 path — perfgate-validated), bounding the gauge's slop and the
/// tick's lateness to 4 iterations. Idle spins keep the 256 counter.
pub(crate) const TICK_CHECK_WORK_ITERS: u32 = 4;

/// Store-only replay of one logged frame — the body shared by both
/// reactors' AOF replay and the reshard merge. Replay never re-logs /
/// re-pushes, so nothing downstream consumes a propagation override; a
/// legacy AOF can still carry a raw `SPOP` frame whose dispatch sets
/// one — drop it per frame so it can't leak into the first live
/// command on the shard.
pub(crate) fn replay_dispatch<C: Commands, A: ArgvView + ?Sized>(
    commands: &C,
    store: &mut kevy_store::Store,
    args: &A,
) {
    let _replaying = crate::replication_gate::RecordApplyGuard::enter();
    commands.dispatch(store, args);
    crate::propagation::discard_override();
}

/// If `args` is an AOF `SELECT <db>` marker frame (multi-DB logs carry one
/// whenever the db changes — the shape Redis uses), return the db index.
pub(crate) fn select_marker<A: ArgvView + ?Sized>(args: &A) -> Option<u16> {
    if args.len() == 2 && args.get(0).is_some_and(|v| v.eq_ignore_ascii_case(b"SELECT")) {
        std::str::from_utf8(args.get(1)?).ok()?.parse().ok()
    } else {
        None
    }
}

/// Apply an admin frame during AOF replay / replica apply that has no
/// embedded dispatch route: `SELECT <db>` moves the cursor, `SWAPDB a b`
/// swaps the two dbs, `FLUSHDB` flushes the cursor db, `FLUSHALL` every db.
/// Returns `true` when the frame was consumed here.
pub(crate) fn replay_admin_frame<A: ArgvView + ?Sized>(
    dbs: &mut crate::dbs::DbStores,
    args: &A,
) -> bool {
    if let Some(db) = select_marker(args) {
        if db < dbs.databases() {
            dbs.set_cur(db);
        }
        return true;
    }
    // FLUSHALL reaches the log on every shard (fan-out); replay must clear
    // every db, not just the cursor's — the embedded FLUSHALL arm only
    // flushes the deref'd cursor db.
    if args.len() <= 2
        && args.get(0).is_some_and(|v| v.eq_ignore_ascii_case(b"FLUSHALL"))
        && args
            .get(1)
            .is_none_or(|v| v.eq_ignore_ascii_case(b"ASYNC") || v.eq_ignore_ascii_case(b"SYNC"))
    {
        dbs.flush_all();
        return true;
    }
    // FLUSHDB: the intercept logs it behind a `SELECT <db>` marker when the
    // db changed, so the cursor is the flushed db at replay.
    if args.len() <= 2
        && args.get(0).is_some_and(|v| v.eq_ignore_ascii_case(b"FLUSHDB"))
        && args
            .get(1)
            .is_none_or(|v| v.eq_ignore_ascii_case(b"ASYNC") || v.eq_ignore_ascii_case(b"SYNC"))
    {
        let db = dbs.cur();
        if db < dbs.databases() {
            dbs.store_mut(db).flushall();
        }
        return true;
    }
    // `MOVE key db` frames: move the key from the current db to db.
    if args.len() == 3 && args.get(0).is_some_and(|v| v.eq_ignore_ascii_case(b"MOVE")) {
        let parse = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u16>().ok();
        if let (Some(key), Some(to_db)) = (args.get(1), args.get(2).and_then(|x| parse(x)))
            && to_db < dbs.databases()
            && to_db != dbs.cur()
            && !dbs.store_mut(to_db).key_exists(key)
            && let Some((v, ttl)) = dbs.take_with_ttl(key)
        {
            dbs.store_mut(to_db).put_with_ttl(key.to_vec(), v, ttl);
        }
        return true;
    }
    if args.len() == 3 && args.get(0).is_some_and(|v| v.eq_ignore_ascii_case(b"SWAPDB")) {
        let parse = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u16>().ok();
        if let (Some(a), Some(b)) =
            (args.get(1).and_then(|x| parse(x)), args.get(2).and_then(|x| parse(x)))
            && a < dbs.databases()
            && b < dbs.databases()
        {
            dbs.swap(a, b);
        }
        return true;
    }
    false
}

impl<C: Commands> Shard<C> {
    /// Tell the command set the two things about this shard it cannot
    /// otherwise learn: which shard it is, and where this runtime
    /// writes.
    ///
    /// One method because both reactors need both, and two lines
    /// repeated in two entry points is how the second one comes to be
    /// missing the day a third is added.
    pub(crate) fn announce_to_commands(&self) {
        self.commands.on_shard_start(self.id);
        self.commands.on_data_dir(&self.data_dir);
    }

    /// Owning shard of `key` under this server's routing scheme.
    #[inline]
    pub(crate) fn shard_of(&self, key: &[u8]) -> usize {
        self.route_of(key).0
    }

    /// The shard owning `key`, and its keyspace hash when routing computed
    /// it on the way.
    // every keyed command routes, and a call costs about as much as the
    // routing of an untagged key
    #[allow(clippy::inline_always)]
    #[inline(always)]
    pub(crate) fn route_of(&self, key: &[u8]) -> (usize, Option<u64>) {
        let routing = if self.cluster.is_some() { Routing::Slots } else { Routing::KevyHash };
        crate::reduce::route_of(key, self.nshards, routing)
    }

    /// This shard's snapshot file: `<data_dir>/dump-<id>.rdb`.
    pub(crate) fn snapshot_path(&self) -> PathBuf {
        kevy_persist::layout::snapshot_path(&self.data_dir, self.id)
    }

    /// This shard's append-only log: `<data_dir>/aof-<id>.aof`.
    pub(crate) fn aof_path(&self) -> PathBuf {
        kevy_persist::layout::aof_path(&self.data_dir, self.id)
    }

    // Busy-poll reactor main loop — per-iter overhead is the proven
    // perf-sensitive surface here (measured: per-iter
    // amortization moves throughput where per-op µs shaving does not);
    // stage extraction risks codegen change for zero readability win.
    // LOC-WAIVER: busy-poll reactor main loop (per-iter perf-sensitive).
    pub(crate) fn run(mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
        self.announce_to_commands();
        // Restore: the snapshot the AOF continues (none under a rewritten,
        // complete AOF), then the AOF. Replay goes straight to the store
        // (no re-logging).
        // Row segments are truth: load the registered set FIRST — a
        // v7 snapshot's stub records and the AOF's SEGMENTED frames
        // both resolve against it.
        if let Err(e) = self.store.enable_seg_rows_all(&self.data_dir, self.id) {
            return Err(io::Error::other(format!("shard {}: {e}", self.id)));
        }
        self.load_boot_snapshot()?;
        if self.aof.is_some() {
            let aof_path = self.aof_path();
            let commands = &self.commands;
            let data_dir = &self.data_dir;
            let shard_id = self.id;
            let dbs = &mut self.store;
            // In-replay demotion: dispatch already runs the
            // per-write demote hook, but a K-frame watermark drain
            // backstops it so a bigger-than-budget log can never
            // outrun the batch budget while the reactor is not yet up.
            let mut frames: u64 = 0;
            let mut torn: Option<kevy_store::SegRowsError> = None;
            let apply = |args: kevy_persist::Argv| {
                // Multi-DB: SELECT markers move the replay cursor, SWAPDB
                // frames swap dbs (later frames land in the right keyspace).
                if replay_admin_frame(dbs, &args) {
                    return;
                }
                if let Some(f) = kevy_persist::segmented_frame(&args) {
                    // The stitch frame re-does a hot-layer eviction; a
                    // manifest that does not hold the segment means the
                    // truth set was damaged — finish the walk, then
                    // refuse startup by name instead of dropping rows.
                    if let Err(e) = dbs.apply_segmented_cur(data_dir, shard_id, f) {
                        torn.get_or_insert(e);
                    }
                    return;
                }
                replay_dispatch(commands, dbs, &args);
                frames += 1;
                if frames.is_multiple_of(kevy_persist::REPLAY_DEMOTE_INTERVAL) {
                    dbs.demote_all_to_watermark();
                }
            };
            let report = if self.replay_resync {
                kevy_persist::replay_aof_resync(&aof_path, apply)?
            } else {
                replay_aof(&aof_path, apply)?
            };
            if let Some(e) = torn {
                return Err(io::Error::other(format!("shard {}: {e}", self.id)));
            }
            self.commands.on_replay_report(report.dropped_bytes, report.corrupt);
        }
        self.finish_restore();

        // Off-accept-set shards have no listener (None); skip register.
        let listener_fd = if let Some(l) = &self.listener {
            l.set_nonblocking()?;
            self.poller.add(l.raw(), kevy_sys::Interest::READ)?;
            l.raw()
        } else {
            -1
        };
        self.poller.add(self.waker.read_fd(), kevy_sys::Interest::READ)?;
        // S3: queued appends + writer thread (fsync off the reactor);
        // no-op when opted out or without an AOF.
        self.epoll_aof_setup();
        // -1 never matches an event fd, so the cluster-off loop below pays
        // one dead integer compare per event and nothing else.
        let mut cluster_fd = -1;
        if let Some(cl) = &self.cluster_listener {
            cl.set_nonblocking()?;
            if self.arms_accept {
                self.poller.add(cl.raw(), kevy_sys::Interest::READ)?;
            }
            cluster_fd = cl.raw();
        }
        // Same trick for the unix-domain listener, which only shard 0
        // holds. Registering it here is what this reactor was missing:
        // the socket is bound before any shard spawns, so a client's
        // connect() lands in the backlog and then waits forever for an
        // accept that only the io_uring reactor ever performed.
        let mut unix_fd = -1;
        if let Some(un) = &self.unix_listener {
            un.set_nonblocking()?;
            if self.arms_accept {
                self.poller.add(un.raw(), kevy_sys::Interest::READ)?;
            }
            unix_fd = un.raw();
        }
        // Same "fd or -1" trick for the replication listener (per Issue
        // Ledger I2 — per-shard, deterministic ports). Replication-off
        // pays one dead integer compare per event and nothing more.
        let mut replication_fd = -1;
        if let Some(rl) = &self.replication_listener {
            rl.set_nonblocking()?;
            self.poller.add(rl.raw(), kevy_sys::Interest::READ)?;
            replication_fd = rl.raw();
        }
        let waker_fd = self.waker.read_fd();
        let me = self.id;
        // Server-as-replica: let the runner thread's sends interrupt a
        // parked poll — see replica_inbox.rs's wake contract.
        if let Some(rx) = &self.replica_inbox {
            rx.attach_waker(Arc::clone(&self.waker));
        }

        let mut tick_interval = match self.commands.shard_tick_interval_ms() {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        };
        let mut last_tick = Instant::now();
        let mut slow = crate::slow_iter::SlowIter::new();
        let mut tick_check_counter: u32 = 0;

        let mut idle_spins: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            // Busy-poll while there's recent work — a cross-core hop then costs
            // no syscall. Park (blocking wait) once we've been idle a while.
            let spinning = idle_spins < self.spin_limit;
            let timeout = if spinning {
                Some(0)
            } else {
                // Close the park/wake race: the fence inside
                // `publish_parked` pairs with `fence_before_wake_scan` in
                // every other shard's `flush_wakes`, so any push that lands
                // BEFORE the drain below is either (a) seen by
                // `drain_inbound` here OR (b) the peer's parked-load saw
                // `true` and a wake syscall is on the way. Without the
                // fence, the lost-wake window was bounded by
                // `PARK_TIMEOUT_MS` (50 ms) — the blocking wait below is
                // now defense-in-depth (covers a missed eventfd write, OS
                // scheduling glitch, etc.). Loom-verified: the functions
                // called here are the ones `tests/loom.rs` schedules.
                park_fence::publish_parked(&self.parked[me]);
                if self.drain_inbound()? {
                    park_fence::clear_parked(&self.parked[me]);
                    self.flush_backlog();
                    self.flush_dirty()?;
                    self.flush_wakes();
                    idle_spins = 0;
                    continue;
                }
                Some(self.park_timeout_ms)
            };

            slow.begin();
            self.poller.wait(&mut self.events, timeout)?;
            slow.mark("poll");
            if !spinning {
                park_fence::clear_parked(&self.parked[me]);
            }

            let events_seen = self.events.len();
            let mut ticked = false;
            let mut did_work = !self.events.is_empty();
            if did_work {
                // Redis-style `updateCachedTime`: refresh the store's coarse
                // clock once per batch, so the per-command read path's lazy
                // expiry skips its own `Instant::now()` (amortized over the
                // whole batch of events processed below).
                self.store.refresh_clock();
                // mem::take only when there's actually work, avoids two Vec
                // moves per empty iter (timeout=Some(0) often returns 0).
                let events = std::mem::take(&mut self.events);
                for ev in &events {
                    if ev.fd == listener_fd {
                        self.accept_ready(Accepted::Compat)?;
                    } else if ev.fd == cluster_fd {
                        self.accept_ready(Accepted::Cluster)?;
                    } else if ev.fd == unix_fd {
                        self.accept_ready(Accepted::Unix)?;
                    } else if ev.fd == replication_fd {
                        self.accept_ready_replication()?;
                    } else if ev.fd == waker_fd {
                        self.waker.drain();
                    } else if let Some(&conn_id) = self.fd_to_conn.get(&ev.fd) {
                        if ev.readable || ev.hup {
                            self.conn_readable(conn_id)?;
                        } else if ev.writable {
                            self.flush_conn(conn_id)?;
                        }
                    } else if let Some(idx) = self.replica_index_by_fd(ev.fd) {
                        let readable = ev.readable || ev.hup;
                        if readable && let Err(e) = self.replica_readable(idx) {
                            self.replica_io_failed(idx, "read", &e);
                        }
                        // A handshake `+ACK` is small (≤ 30 B) and
                        // usually fits in the first non-blocking write,
                        // so try the drain unconditionally before
                        // requesting write-readiness. If it short-writes,
                        // `replica_writable` is a no-op until the poller
                        // signals writability (the write-readiness
                        // re-arm covers the short-write case; in
                        // practice `+ACK` drains in one syscall on
                        // every OS we test).
                        if let Err(e) = self.replica_writable(idx) {
                            self.replica_io_failed(idx, "write", &e);
                        }
                    }
                }
                self.events = events;
                slow.mark("events");
                // Drop conns that hit Closed mid-event (handshake
                // error / peer EOF / `+ACK` drained in this batch's
                // terminal state). Reaping before the next poll
                // prevents a closed fd from re-firing on epoll level-
                // triggered backends. E9: standalone shards skip even
                // the gate inside the function.
                if !self.replicas.is_empty() {
                    self.reap_closed_replicas();
                }
            }

            // The closing ready-set is an io_uring-reap accelerator;
            // this backend closes via the dirty-flush path instead,
            // so the QUIT / CLIENT KILL dispatch sites' pushes would
            // otherwise accumulate forever (8 bytes per closed conn,
            // unbounded on a long-running server).
            self.closing_uring_conns.clear();

            // Messages from other cores (forwarded requests + replies to ours).
            if self.drain_inbound()? {
                did_work = true;
            }
            if !self.held.is_empty() {
                self.resume_held()?;
            }
            self.install_adopted_polled()?;
            slow.mark("inbound");
            // Re-push anything that overflowed a full ring last iteration.
            self.flush_backlog();
            // Hook messages no client waits on (expiry, replica apply,
            // backfill) go out unacknowledged, ahead of the batch below.
            self.send_ext(false);
            // Send this iteration's batched single-key dispatches (one per target).
            self.flush_requests();
            // Send this iteration's batched pub/sub deliveries (one per target).
            self.flush_publish();
            // Flush subscribers a PUBLISH wrote to this iteration.
            self.flush_dirty()?;
            // One wakeup per touched (and parked) target this iteration.
            self.flush_wakes();
            slow.mark("flush");
            // Ship the per-shard bio-drop batch to the bio
            // thread BEFORE the AOF fsync window. Same rationale as the
            // io_uring path: don't let a pending fsync stall pin the
            // batch in RSS, and bound the per-iter drop latency
            // window. Empty-buffer fast path = predicted-not-taken
            // length check, sub-ns on iters that did no overwrite.
            // Multi-DB: drop batches flush per db, not just the cursor's.
            for db in 0..self.store.databases() {
                self.store.set_cur(db);
                self.store.flush_pending_drops();
            }
            self.store.set_cur(0);
            slow.mark("drops");
            // AOF pump: writer-lane reap/submit/fsync (S3), or the
            // classic synchronous EverySec window when the lane is off.
            self.epoll_aof_tick();
            slow.mark("aof");
            // Active TTL reaper / shard housekeeping. Skip the wall-clock
            // read on most iters: in busy-poll the tick fires at 10 Hz
            // with negligible overhead (counter saturates in ~us, then
            // checks elapsed). In park mode each iter is already ≥ 1 ms
            // so the throttle would delay the tick by 256 iters × 50 ms
            // = ~12 s on a fully-idle server — bypass the counter when
            // we just came back from a parking wait so the tick fires
            // at every park iteration regardless of recent traffic.
            // Work iterations check the clock every TICK_CHECK_WORK_ITERS
            // (see the constant's doc — saturation gate, load-proportional);
            // idle spins keep the 256-iter counter, parked wakes bypass it.
            if let Some(iv) = tick_interval {
                tick_check_counter = tick_check_counter.wrapping_add(1);
                if tick_check_counter >= self.tick_check_every
                    || !spinning
                    || (did_work && tick_check_counter >= TICK_CHECK_WORK_ITERS)
                {
                    tick_check_counter = 0;
                    let now = Instant::now();
                    // BLOCK reactor: fire timeouts every tick gate (not gated
                    // by `iv`), so a `BLPOP k 0.5` resolves on the next 50ms
                    // park instead of the next user-level shard tick.
                    self.tick_blocked_timeouts();
                    self.tick_xshard_timeouts();
                    // WAIT / REPL.WAIT deadline sweep — same
                    // cadence as the BLOCK timeout reactor above.
                    self.tick_repl_waiters();
                    slow.mark("t:timeouts");
                    let gap = now.duration_since(last_tick);
                    if gap >= iv {
                        // Tail observability: how late is this tick? A
                        // reactor stalled 250 ms in one iteration fires
                        // its next tick ~250 ms over the interval — the
                        // gauge IS the single-iteration upper bound,
                        // measured without touching the per-iter path.
                        self.commands.on_tick_gap((gap - iv).as_micros() as u64);
                        // Tick gauges report db0 deterministically (multi-DB: the INFO
                        // keyspace line is db0's; per-db lines are Phase 2).
                        self.store.set_cur(0);
                        self.commands.on_shard_tick(&mut self.store);
                        // Multi-DB: publish per-db keyspace gauges for INFO keyspace.
                        let ndbs = self.store.databases();
                        if ndbs > 1 {
                            let dbs: Vec<(u16, u64, u64)> = (0..ndbs)
                                .map(|db| {
                                    let st = self.store.store(db);
                                    (db, st.dbsize() as u64, st.expires_count() as u64)
                                })
                                .collect();
                            self.commands.on_db_gauges(&dbs);
                        }
                        slow.mark("t:shard_tick");
                        self.drain_tick_frames();
                        // Multi-DB: the notify/expiry drains run per db,
                        // not just on the cursor (db0).
                        for db in 0..self.store.databases() {
                            self.store.set_cur(db);
                            self.drain_store_notify();
                            self.drain_expired_keys();
                        }
                        self.store.set_cur(0);
                        slow.mark("t:drains");
                        slow.mark("t:expired");
                        self.apply_live_runtime_config(&mut tick_interval);
                        slow.mark("t:live_cfg");
                        self.epoll_tick_persist();
                        slow.mark("t:persist");
                        self.tick_conn_gauge();
                        self.enforce_output_limit();
                        slow.mark("t:gauge_limit");
                        // Replication slot expiry:
                        // drop slots whose reconnect window has passed.
                        // No-op short-circuits when replication is off or
                        // no slot has been recorded yet.
                        self.tick_replication_slots(now);
                        // ROLE / INFO replication:
                        // publish master_repl_offset + connected_replicas
                        // count to the embedder. No-op when replication
                        // is off.
                        self.tick_replication_view();
                        // Backlog watermark: drop
                        // frames every consumer has moved past so the
                        // backlog reclaims space proactively. No-op
                        // when replication is off / no consumers yet.
                        self.tick_replication_watermark();
                        last_tick = now;
                        ticked = true;
                    }
                }
            }
            slow.mark("tick");

            // Replication producer pump.
            // OUTSIDE the did_work block: the
            // heartbeat (1s) and the ACK drain must run on idle iters
            // too — a parked-and-woken shard with zero events still
            // owes its replicas a pulse. Cost when replication is off
            // stays one branch (E9 gating preserved).
            if self.replicate.is_some() || !self.replicas.is_empty() {
                self.pump_replication()?;
            }
            // Server-as-replica: drain events from the replica runner
            // thread and apply them. Every iteration, not the interval
            // tick — at tick cadence the 1024-event budget caps apply
            // throughput at ~10k frames/s, far under what an upstream
            // primary emits (the repligate drain regression). Gated at
            // the call site like the pump above.
            if self.replica_inbox.is_some() {
                self.drain_replica_inbox();
            }
            slow.mark("repl");
            if slow.enabled() {
                slow.finish(
                    self.id,
                    format_args!("events={events_seen} ticked={ticked} conns={}", self.conns.len()),
                );
            }
            // A non-empty backlog means a peer ring is full: keep spinning so we
            // re-attempt the flush (and keep draining inbound to unblock peers).
            let has_backlog = self.backlog.iter().any(|b| !b.is_empty());
            // Stay-hot-while-inflight, epoll symmetry — the
            // uring reactor gained this
            // first: with forwarded cross-shard requests
            // outstanding, replies land within ~one RTT, so hold the
            // spin rung instead of paying park+wake per reply batch.
            // Bounded: inflight only drains (owner answers) or the
            // conn dies.
            idle_spins = if did_work || has_backlog || self.xshard_inflight > 0 {
                0
            } else {
                idle_spins.saturating_add(1)
            };
        }
        // Exit sequence: an optional `SHUTDOWN SAVE` snapshot, then
        // drain any in-flight bg persist job so a `Op::Save` that
        // returned `+OK` to a client still lands its `dump-{i}.rdb`
        // rename + AOF reset (the commit phase otherwise runs on the
        // next tick, which won't happen after `stop=true`), then the
        // final AOF fsync + feed marker. See [`Self::shutdown_drain`].
        self.epoll_aof_settle();
        self.shutdown_drain();
        Ok(())
    }

    // `apply_live_runtime_config` + `maybe_auto_rewrite_aof` (the
    // per-tick housekeeping) live in [`crate::shard_tick`] — same
    // `impl<C: Commands> Shard<C>`, split out so this file stays under
    // the 500-LOC house rule.

    // The outbound transport half (`flush_wakes` / `flush_dirty` /
    // `send_to` / `flush_backlog` / `flush_conn`) lives in
    // [`crate::shard_flush`] — same `impl<C: Commands> Shard<C>`, split
    // out so this file stays under the 500-LOC house rule.
}
