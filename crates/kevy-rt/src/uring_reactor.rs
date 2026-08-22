//! Linux io_uring **completion**-based reactor for a [`Shard`] — the Phase-2
//! alternative to the readiness loop in [`crate::shard`].
//!
//! Same command semantics (it reuses `handle_command`, `exec_op`, `fold`,
//! `send_to`, the seq-ordered reply ring, and the cross-core kevy-ring drain);
//! only the I/O layer changes: instead of epoll telling us an fd is ready and
//! then issuing a `read`/`write` syscall each, we **submit** accept/read/write
//! SQEs and reap their CQEs, batching socket I/O through one `io_uring_enter`.
//!
//! Opted into on Linux via `KEVY_IO_URING=1` (see [`crate::Runtime`]); the
//! readiness reactor stays the default and the macOS path.
//!
//! Scope: accept + per-conn read → dispatch → write, plus the cross-core
//! drain. Idle handling is a spin → nap → park ladder; the park rung is the
//! epoll reactor's park translated to the ring: `parked[me]` + a waker-pipe
//! read SQE + an `IORING_OP_TIMEOUT` bound, all satisfied by one blocking
//! `submit_and_wait(1)`. Pub/sub's direct `flush_conn` write is not yet
//! wired here (no pub/sub in `sharded`).

use crate::Commands;
use crate::conn::Conn;
use crate::shard::Shard;
use crate::uring_conn::ParkState;
pub(crate) use crate::uring_conn::UringConn;
pub(crate) use crate::uring_setup::{URING_ENTRIES, build_uring, io_uring_available};
use kevy_map::KevyMap;
use kevy_sys::Socket;
use kevy_uring::{Completion, IoUring};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// SQPOLL is OFF by default in the shard reactor — it spawns one kernel
// poll thread per shard, each spinning at ~100% on the same core set as
// the shard threads, halving effective CPU (see the SQPOLL entries in
// bench/PERF-ATTACK-LOG for the measured regressions). The
// `KEVY_SQPOLL=1` env switch (see `uring_setup::build_uring`) opts in
// for A/B measurement on layouts with spare cores; the wire-level
// support lives in `kevy_uring::IoUring::new_sqpoll`.
/// Busy-poll iterations after the last work before yielding the core (mirrors
/// the epoll reactor's `SPIN_LIMIT`). Keeps -c1 latency low without spinning a
/// quiet shard at 100% forever.
const URING_SPIN_LIMIT: u32 = 256;
/// Nap rung (batch-gated, see the idle ladder): deaf-sleep length. Long
/// enough for all 7 origin shards to enqueue another round of forwards,
/// short enough that a once-per-burst straggler barely notices.
const NAP_US: u64 = 200;
/// Minimum size of the previous inbound drain for the nap rung to arm.
/// Sequential -c1 traffic drains 1 message per request and must never
/// nap (that was the 15× -c1 bug the batch gate exists to prevent).
const NAP_BATCH_MIN: usize = 4;
// The `user_data` op-tag layout (`OP_*` / `CONN_MASK`), the writev
// iovec cap, and the special-cased errnos live in [`crate::uring_ops`]
// — split out so this file stays under the 500-LOC house rule.
// Re-exported so the sibling uring modules keep their
// `crate::uring_reactor::…` paths.
pub(crate) use crate::uring_ops::{
    CONN_MASK, ENOBUFS, MAX_IOVECS_PER_WRITEV, OP_ACCEPT, OP_ACCEPT_CL, OP_ACCEPT_UN, OP_AOF,
    OP_BIG_CANCEL, OP_BIG_READ, OP_RECV, OP_TIMEOUT, OP_WAKER, OP_WRITE,
};

impl<C: Commands> Shard<C> {
    /// Completion-based run loop (Linux io_uring). Mirrors [`Shard::run`] but
    /// drives socket I/O through io_uring instead of the readiness poller.
    /// The ring pair comes from [`build_uring`] at the spawn site (see the
    /// fallback rationale there).
    // Busy-poll reactor main loop — per-iter overhead is the proven
    // perf-sensitive surface here (measured: per-iter
    // amortization moves throughput where per-op µs shaving does not);
    // stage extraction risks codegen change for zero readability win.
    // LOC-WAIVER: busy-poll reactor main loop (per-iter perf-sensitive).
    pub(crate) fn run_uring(
        mut self,
        ring_pair: (IoUring, kevy_uring::ProvidedBufRing),
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        self.prepare_uring_shard()?;
        self.uring_aof_setup();

        // One provided-buffer ring per shard feeds every conn's multishot recv
        // (needs Linux 5.19+; the epoll reactor is the fallback for older
        // kernels AND for per-shard setup failure — see `build_uring`).
        let (mut ring, mut pbuf) = ring_pair;
        // Replication listener accept must NOT block the reactor.
        // The epoll path sets this in `shard::run` via `poller.add`-side
        // setup; the io_uring path originally didn't. `accept_ready_replication`
        // (called per-tick) loops until `WouldBlock` — which never fires
        // on a blocking socket, so the first `accept()` call stalls the
        // entire shard until a replica connects. Root cause of an earlier
        // finding: primary kevy with `[replication] role = "primary"` was
        // unresponsive to client PING under the io_uring reactor.
        if let Some(rl) = &self.replication_listener {
            rl.set_nonblocking()?;
        }
        let mut io: KevyMap<u64, UringConn> = KevyMap::new();
        let mut accept_inflight = false;
        // Starts "in flight" when cluster mode is off, so the arm loop never
        // preps an accept on a listener that doesn't exist.
        let mut cl_accept_inflight = self.cluster_listener.is_none();
        // UDS: only shard 0 may hold a unix listener.
        let mut un_accept_inflight = self.unix_listener.is_none();
        let mut comps: Vec<Completion> = Vec::with_capacity(URING_ENTRIES as usize);
        let mut idle_spins: u32 = 0;
        let stall_dump_every = crate::uring_stalldump::stall_dump_interval();
        let mut last_stall_dump = Instant::now();
        // Nap rung (restored, batch-gated): size of the last
        // non-empty inbound drain + whether this idle episode already
        // napped. See the idle-ladder comment below.
        let mut last_inbound_batch: usize = 0;
        let mut napped = false;
        let mut park = ParkState::default();
        let mut woke_from_park = false;

        // Active reaper / hot-config / auto-rewrite tick — same shape as the
        // epoll path in `shard::run`. Without this branch the io_uring
        // reactor would silently skip TTL active expiry, auto-AOF-rewrite,
        // and `CONFIG SET` propagation (lazy expiry on access still works).
        let mut tick_interval = match self.commands.shard_tick_interval_ms() {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        };
        let mut last_tick = Instant::now();
        let mut tick_check_counter: u32 = 0;
        // 1/16 cadence: reap is 5.7 % of -c50 -P16 SET CPU, rarely fruitful.
        let mut reap_counter: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            // One multishot accept SQE per listener stays
            // armed across many connections. The kernel re-fires it per
            // incoming conn, each CQE carrying the new fd in `res` and
            // `IORING_CQE_F_MORE` set while still armed. We only re-submit
            // when F_MORE clears (kernel dropped the multishot — listener
            // close, ENOBUFS, etc.). Zero -c1 cost (one persistent conn
            // takes one accept ever); cuts the per-accept SQE under
            // high-conn-churn workloads.
            // `arms_accept = false` shards skip every accept arm so
            // the kernel SO_REUSEPORT layer routes new conns only to the
            // armed subset. Off-accept-set shards still receive cross-shard
            // dispatched work via drain_inbound below.
            if self.arms_accept
                && !accept_inflight
                && let Some(l) = &self.listener
            {
                accept_inflight = ring.prep_accept_multishot(l.raw(), OP_ACCEPT);
            }
            if self.arms_accept
                && !cl_accept_inflight
                && let Some(cl) = &self.cluster_listener
            {
                cl_accept_inflight = ring.prep_accept_multishot(cl.raw(), OP_ACCEPT_CL);
            }
            if self.arms_accept
                && !un_accept_inflight
                && let Some(un) = &self.unix_listener
            {
                un_accept_inflight = ring.prep_accept_multishot(un.raw(), OP_ACCEPT_UN);
            }
            self.uring_arm_conns(&mut ring, &mut io, pbuf.group());

            ring.submit_and_wait(0)?; // submit queued SQEs; reap is non-blocking
            comps.clear();
            ring.for_each_completion(|c| comps.push(c));

            // Redis-style `updateCachedTime`: refresh the store's coarse clock
            // once per batch so per-command lazy expiry skips `Instant::now()`.
            if !comps.is_empty() {
                self.store.refresh_clock();
            }
            // Park-administrative CQEs (waker / timeout) must not count as
            // work: an idle shard's bounded park produces one of them every
            // `park_timeout_ms`, and treating that as work would reset the
            // idle ladder into a 100 %-CPU spin burst per tick.
            let mut io_work = false;
            // E11: dispatch loop body. RECV / WRITE dominate at -c1
            // (every request is one recv + one write); ACCEPT / WAKER /
            // TIMEOUT fire once at conn start and at park transitions.
            // Reorder so the hot arms are first AND tag the cold tail
            // with `#[cold]` so LLVM keeps it off the predicted-taken
            // fall-through. `perf record -e branch-misses` before E11
            // showed the closure was 33% of all branch mispredictions —
            // the per-completion dispatch was a major source.
            #[cold]
            #[inline(never)]
            fn cold_path_hint() {}
            for c in &comps {
                let op = c.user_data & !CONN_MASK;
                let cid = c.user_data & CONN_MASK;
                match op {
                    // `true` = the watermark moved, so a held reply is
                    // waiting on the next arming pass and this iteration
                    // must not park. See `uring_aof_on_cqe`.
                    OP_AOF => io_work |= self.uring_aof_on_cqe(c.user_data, c.res),
                    OP_RECV => {
                        io_work = true;
                        self.uring_on_recv(cid, c, &mut io, &mut pbuf);
                    }
                    OP_WRITE => {
                        io_work = true;
                        self.uring_on_write(cid, c.res, &mut io);
                        // A write CQE — even a fully-drained one —
                        // wants an arm visit so a chunked-writev tail
                        // (pub/sub burst > IOV_MAX) gets its next
                        // chunk out. Cheap when nothing remains: the
                        // visit's `needs_more` check drops the conn
                        // out of the queue.
                        self.mark_arm_pending(cid, &mut io);
                    }
                    OP_ACCEPT | OP_ACCEPT_CL | OP_ACCEPT_UN => {
                        cold_path_hint();
                        let cluster = op == OP_ACCEPT_CL;
                        let is_unix = op == OP_ACCEPT_UN;
                        // Only clear the in-flight flag when the
                        // multishot terminates (F_MORE clear). While
                        // F_MORE is set the kernel still has the SQE
                        // armed and will re-fire on the next conn — no
                        // need to re-submit, and the top-of-loop
                        // re-arm gate would queue a duplicate.
                        if !c.has_more() {
                            if cluster {
                                cl_accept_inflight = false;
                            } else if is_unix {
                                un_accept_inflight = false;
                            } else {
                                accept_inflight = false;
                            }
                        }
                        io_work = true;
                        if c.res >= 0 {
                            // SAFETY: a freshly accepted fd we now own.
                            let sock = unsafe { Socket::from_raw_fd(c.res) };
                            // Refuse client conns past max_clients_per_shard
                            // (cluster-bus links exempt as infrastructure).
                            if !cluster
                                && self.max_clients_per_shard > 0
                                && self.conns.len() >= self.max_clients_per_shard
                            {
                                self.rejected_connections =
                                    self.rejected_connections.saturating_add(1);
                                drop(sock); // close fd immediately
                                continue;
                            }
                            // TCP_NODELAY doesn't apply to AF_UNIX; skip for UDS.
                            if !is_unix {
                                let _ = sock.set_nodelay();
                            }
                            let ncid = self.next_conn_id;
                            self.next_conn_id += self.conn_id_step;
                            let mut conn = Conn::new(sock);
                            conn.cluster = cluster;
                            self.conns.insert(ncid, conn);
                            let mut uc = UringConn::new();
                            // New conn needs an arm visit so its
                            // multishot recv gets queued.
                            uc.arm_queued = true;
                            io.insert(ncid, uc);
                            self.arm_pending.push(ncid);
                            // Client connections only — cluster-bus is internal.
                            if !cluster {
                                self.commands.on_connection();
                            }
                        }
                    }
                    OP_WAKER => {
                        cold_path_hint();
                        park.waker_armed = false;
                        // The read took ≤ 8 bytes; clear any pile-up beyond it.
                        self.waker.drain();
                    }
                    OP_TIMEOUT => {
                        cold_path_hint();
                        park.timeout_inflight = false;
                    }
                    OP_BIG_CANCEL => {
                        cold_path_hint();
                        io_work = true;
                        self.uring_on_big_arg_cancel(cid, c.res, &mut io);
                    }
                    OP_BIG_READ => {
                        cold_path_hint();
                        io_work = true;
                        self.uring_on_big_arg_read(cid, c.res, &mut io);
                    }
                    _ => {
                        cold_path_hint();
                    }
                }
            }

            // Cross-core: forwarded requests + replies (output accumulates; the
            // io_uring write path below flushes it).
            let did_inbound = self.uring_drain_inbound();
            // `self.dirty` is no longer cleared here —
            // pub/sub deliver paths push into it and `uring_arm_conns`
            // drains it into `arm_pending` on the next iter. The prior
            // shape relied on arm_conns scanning every conn each iter
            // (idle conns were a ~5 ns fast-skip), so the marks could be
            // discarded; with the dirty-set arm loop, the marks are
            // load-bearing.
            self.flush_backlog();
            self.flush_requests();
            self.flush_publish();
            self.flush_wakes();
            // Ship the per-shard bio-drop batch to the bio
            // thread BEFORE the AOF fsync window. Two reasons:
            // (1) a pending fsync stall (EverySec / Always) would
            //     otherwise pin a batch's worth of `Box<Value>` heap
            //     in this shard's RSS for the fsync duration;
            // (2) keeps the per-iter drop latency window bounded to
            //     one reactor iteration regardless of AOF state.
            // Empty-buffer fast path = predicted-not-taken length
            // check, so the cost on iters that did no overwrite is
            // sub-ns.
            self.store.flush_pending_drops();
            self.uring_aof_tick(&mut ring);
            reap_counter = reap_counter.wrapping_add(1);
            if reap_counter & 0xF == 0 {
                self.uring_reap_closed(&mut io);
            }

            // Tick path: throttled wall-clock check, then the hot-config /
            // active-reaper / auto-rewrite trio. Same throttle as epoll
            // (256-iter counter + `tick_interval` elapsed gate).
            if let Some(iv) = tick_interval {
                tick_check_counter = tick_check_counter.wrapping_add(1);
                // `|| woke_from_park`: mirrors the epoll path's `|| !spinning`
                // — parked iterations are ≥ ms apart, so gating them behind
                // the 256-iter counter would delay ticks (and BLPOP/XREAD
                // timeouts) by minutes on an idle shard.
                // Work iterations check the clock every
                // `TICK_CHECK_WORK_ITERS` (the epoll path's saturation
                // gate — see the constant's doc): load-proportional, so a
                // saturated shard's tick fires on schedule and the
                // tick-gap gauge measures stalls, not accumulated busy
                // time. Batch SIZE was tried and refuted (one P16
                // completion carries 16 commands).
                if tick_check_counter >= self.tick_check_every
                    || woke_from_park
                    || ((io_work || did_inbound > 0)
                        && tick_check_counter >= crate::shard_run::TICK_CHECK_WORK_ITERS)
                {
                    tick_check_counter = 0;
                    let now = Instant::now();
                    // BLOCK reactor: same cadence as the epoll path so
                    // BLPOP / XREAD BLOCK timeouts fire identically under
                    // either reactor.
                    self.tick_blocked_timeouts();
                    self.tick_xshard_timeouts();
                    self.uring_maybe_dump_stalled(stall_dump_every, &mut last_stall_dump, now, &io);
                    // WAIT / REPL.WAIT deadline sweep — same
                    // cadence as the BLOCK timeout reactor above.
                    self.tick_repl_waiters();
                    let gap = now.duration_since(last_tick);
                    if gap >= iv {
                        // Tail observability — the epoll twin's comment
                        // applies verbatim: the tick's lateness IS the
                        // single-iteration stall upper bound.
                        self.commands.on_tick_gap((gap - iv).as_micros() as u64);
                        self.commands.on_shard_tick(&mut self.store);
                        self.drain_tick_frames();
                        self.drain_store_notify();
                        self.drain_expired_keys();
                        self.apply_live_runtime_config(&mut tick_interval);
                        self.uring_tick_persist();
                        self.tick_conn_gauge();
                        self.uring_enforce_output_limit(&mut io);
                        self.uring_tick_replication(now);
                        last_tick = now;
                    }
                }
            }

            // Per-iter replication pump: writes streaming
            // frames + drives snapshot ship chunks. Hoist the
            // "is this shard actually doing replication" predicate to
            // the call site so the steady-state standalone workload
            // pays one branch instead of two function-call frames
            // (perf-record measured 1.0% + 1.0% self-time on the empty
            // gates inside the functions; the gate-hoist drops both to
            // 0). If new replication-side work shows up here, audit
            // whether it needs to run on standalone shards too.
            if self.replicate.is_some() || !self.replicas.is_empty() {
                self.pump_replication()?;
                self.reap_closed_replicas();
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

            // Idle ladder — spin, then a BATCH-GATED deaf nap, then park:
            //   1. busy-poll `URING_SPIN_LIMIT` empty iterations, so a -c1
            //      client's next request is reaped immediately;
            //   2. nap (only when the last inbound drain was a real batch,
            //      `>= NAP_BATCH_MIN`, and once per idle episode): a
            //      bounded `thread::sleep(NAP_US)` that lets the 7 origin
            //      shards' forwards accumulate so the owner drains one big
            //      batch per wake instead of park/wake-churning per small
            //      batch;
            //   3. park: io_uring blocking wait, woken by any socket I/O
            //      CQE, the waker pipe, or the bounding timeout. A truly
            //      idle shard costs ~zero CPU.
            //
            // Why the batch gate exists: the original
            // nap was UNCONDITIONAL `thread::sleep(200 µs)` —
            // great for the 8-shard cross-shard shape (aggregation), but
            // wake-deaf, so a sequential -c1 Rust client paid the full
            // 200 µs per request (~4 k ops/s, 15× slower than valkey).
            // A later change removed the rung entirely, fixing -c1
            // but silently costing a measured −18~21 % on the 8-shard
            // shape (`legacy_8sh_set` 9.98M → 7.6M across that single
            // change). The batch gate keeps
            // both: -c1 traffic drains batches of 1 (< NAP_BATCH_MIN) and
            // goes straight to the wake-aware park — its 15 µs steady
            // state is untouched; the cross-shard owner sees large drains
            // and earns the aggregation nap. Worst-case added latency for
            // a lone request that lands right after a burst: one NAP_US,
            // once (the `napped` flag forces park next).
            // Full evidence chain: the legacy8sh owner-starvation
            // PERF-DECOMP note in bench/.
            //
            // A non-empty backlog means a peer ring is full — keep
            // spinning to re-attempt the flush (nothing would wake us
            // when the peer drains).
            woke_from_park = false;
            let has_backlog = self.backlog.iter().any(|b| !b.is_empty());
            // A closing conn's recv is not re-armed → no further CQE;
            // parking would strand its fd half-open, so keep spinning.
            let reap_pending = !self.closing_uring_conns.is_empty();
            if !io_work && did_inbound == 0 && !has_backlog && !reap_pending {
                // Forwarded requests outstanding ⇒ replies land
                // within ~one cross-shard RTT — stay in the spin rung
                // rather than paying a kernel sleep + wake per reply
                // batch. Bounded: inflight can only drain (the owner
                // answers) or the conn dies (folds error responses).
                if self.xshard_inflight > 0 {
                    std::hint::spin_loop();
                    continue;
                }
                idle_spins = idle_spins.saturating_add(1);
                if idle_spins >= URING_SPIN_LIMIT {
                    if !napped && last_inbound_batch >= NAP_BATCH_MIN {
                        std::thread::sleep(Duration::from_micros(NAP_US));
                        napped = true;
                    } else {
                        self.uring_park(&mut ring, &mut park)?;
                        woke_from_park = true;
                        napped = false;
                        last_inbound_batch = 0;
                    }
                } else {
                    // E12: signal the CPU that we are in a spin-wait loop.
                    // Compiles to `PAUSE` on x86 / `YIELD` on ARM. Reduces
                    // power draw, frees pipeline bandwidth for the SMT
                    // sibling, and lowers branch-history pollution from the
                    // outer iter's speculative reads. Cheap when nothing's
                    // arrived; no effect when there IS work since this
                    // branch isn't reached.
                    std::hint::spin_loop();
                }
            } else {
                idle_spins = 0;
                if did_inbound > 0 {
                    last_inbound_batch = did_inbound;
                    napped = false;
                }
            }
        }
        // Exit sequence: land every in-flight AOF chunk FIRST — the
        // shutdown drain below may commit a rewrite (rename + reopen:
        // the old fd closes and its number can be reused by the new
        // file, so a straggler positioned write would corrupt it) and
        // its final `sync_now` flushes the queue in append mode (EOF
        // is only truthful once the positioned writes have landed).
        // Then the usual drain: optional `SHUTDOWN SAVE` snapshot, bg
        // persist completions (so a `+OK` SAVE reply isn't followed by
        // a torn snapshot), final AOF fsync, feed marker — see
        // [`Shard::shutdown_drain`].
        self.uring_aof_drain_exit(&mut ring);
        self.shutdown_drain();
        Ok(())
    }

    // Module map (all on the same `impl<C: Commands> Shard<C>`, only
    // ever called from `run_uring` above; split per the 500-LOC house
    // rule): `uring_arm_conns` → [`crate::uring_arm`]; `uring_on_recv` /
    // `uring_mark_closing` / `uring_on_write` → [`crate::uring_io`];
    // `uring_drain_inbound` + `uring_reap_closed` → [`crate::uring_inbox`];
    // the bounded park → [`crate::uring_park`]; the shared op-tag
    // constants → [`crate::uring_ops`].
}
