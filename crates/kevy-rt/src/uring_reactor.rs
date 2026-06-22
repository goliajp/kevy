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
pub(crate) use crate::uring_conn::UringConn;
use crate::uring_conn::ParkState;
use kevy_persist::{load_snapshot, replay_aof};
use kevy_sys::Socket;
use kevy_uring::{Completion, IoUring};
use kevy_map::KevyMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// SQ/CQ depth per-shard. Paired with `PBUF_ENTRIES` — v1.25 G1/K2 bumped
/// both to fix the c=10 000 cliff (deco-axis-k-c10000).
const URING_ENTRIES: u32 = 2048;
// SQPOLL is NOT wired into the shard reactor — it would spawn one kernel
// poll thread per shard, each spinning at ~100% on the same core set as
// the shard threads, halving effective CPU. See `bench/PERF-ATTACK-LOG-2026-06-20.md`
// (attack D5) for the 2-15× regression measurement and the architectural
// reasoning. The wire-level support stays in `kevy_uring::IoUring::new_sqpoll`
// for callers with single-threaded reactors and spare cores.
/// Busy-poll iterations after the last work before yielding the core (mirrors
/// the epoll reactor's `SPIN_LIMIT`). Keeps -c1 latency low without spinning a
/// quiet shard at 100% forever.
const URING_SPIN_LIMIT: u32 = 256;
// The nap rung was removed (see the idle-ladder comment in `run_uring`).
// URING_NAP_LIMIT / URING_NAP_MICROS / `uring_nap` are gone; spin →
// park is the whole ladder now.
/// Shared provided-buffer ring: 4096 × 16K = 64 MiB/shard. Linux multishot
/// recv terminates on ENOBUFS — must size for max conns (deco-axis-k-c10000).
const PBUF_ENTRIES: u16 = 4096;
const PBUF_SIZE: u32 = 16 * 1024;
const PBUF_GROUP: u16 = 0;
/// `-ENOBUFS`: the buf ring was momentarily empty; just re-arm (don't close).
pub(crate) const ENOBUFS: i32 = 105;

/// **A.4 (v1.25)**: maximum iovec entries packed into one `writev` SQE.
/// Linux caps `writev(2)` (and `IORING_OP_WRITEV`) at `IOV_MAX = 1024`
/// vectors; over the cap the kernel returns `-EINVAL`. The chunked-
/// writev state machine on `UringConn` (`arcs_in_flight` +
/// `write_byte_cap` + `write_inflight_bytes`) submits one chunk per
/// arm_conns iter and drops the processed prefix in the CQE handler,
/// so a 50-sub × 1024-publish pub/sub burst lands in ~2 SQEs per conn
/// (TCP-ordered, never violating IOV_MAX) instead of forcing the
/// in-shard memcpy fallback after the prior 256-arc cap.
pub(crate) const MAX_IOVECS_PER_WRITEV: usize = 1024;

/// Probe whether this host can build the io_uring + provided-buffer ring that
/// [`Shard::run_uring`] needs: `io_uring_setup` not blocked by seccomp (Docker's
/// default profile blocks it) and a kernel new enough for the buf ring (5.19+).
/// Builds and immediately drops a real ring with the same parameters, so a
/// success here means `run_uring` will start. [`crate::Runtime`] calls this once
/// before spawning shards to auto-select io_uring with a graceful epoll fallback
/// — so an unavailable io_uring degrades to epoll instead of failing startup.
pub(crate) fn io_uring_available() -> bool {
    match IoUring::new(URING_ENTRIES) {
        Ok(ring) => ring
            .register_buf_ring(PBUF_ENTRIES, PBUF_SIZE, PBUF_GROUP)
            .is_ok(),
        Err(_) => false,
    }
}

// `user_data` layout: top 3 bits = op, low 61 bits = conn id.
const OP_SHIFT: u32 = 61;
pub(crate) const OP_RECV: u64 = 1 << OP_SHIFT;
pub(crate) const OP_WRITE: u64 = 2 << OP_SHIFT;
const OP_ACCEPT: u64 = 3 << OP_SHIFT;
/// The shard's waker pipe became readable (a peer woke a parked shard).
pub(crate) const OP_WAKER: u64 = 4 << OP_SHIFT;
/// The bounded-park timeout fired (see [`ParkState`]).
pub(crate) const OP_TIMEOUT: u64 = 5 << OP_SHIFT;
/// Accept on the per-shard cluster listener (conns marked for `-MOVED`).
const OP_ACCEPT_CL: u64 = 6 << OP_SHIFT;
const CONN_MASK: u64 = (1 << OP_SHIFT) - 1;

impl<C: Commands> Shard<C> {
    /// Completion-based run loop (Linux io_uring). Mirrors [`Shard::run`] but
    /// drives socket I/O through io_uring instead of the readiness poller.
    pub(crate) fn run_uring(mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
        self.commands.on_shard_start(self.id);
        // Restore: snapshot then AOF replay (same as the readiness path).
        let snap = self.snapshot_path();
        if snap.exists()
            && let Err(e) = load_snapshot(&mut self.store, &snap)
        {
            eprintln!("kevy: shard {} failed to load {}: {e}", self.id, snap.display());
        }
        if self.aof.is_some() {
            let aof_path = self.aof_path();
            let commands = &self.commands;
            let store = &mut self.store;
            replay_aof(&aof_path, |args| {
                commands.dispatch(store, &args);
            })?;
        }

        let mut ring = IoUring::new(URING_ENTRIES)?;
        // One provided-buffer ring per shard feeds every conn's multishot recv
        // (needs Linux 5.19+; the epoll reactor is the fallback for older kernels).
        let mut pbuf = ring.register_buf_ring(PBUF_ENTRIES, PBUF_SIZE, PBUF_GROUP)?;
        let mut io: KevyMap<u64, UringConn> = KevyMap::new();
        let mut accept_inflight = false;
        // Starts "in flight" when cluster mode is off, so the arm loop never
        // preps an accept on a listener that doesn't exist.
        let mut cl_accept_inflight = self.cluster_listener.is_none();
        let mut comps: Vec<Completion> = Vec::with_capacity(URING_ENTRIES as usize);
        let mut idle_spins: u32 = 0;
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
            // B4 (2026-06-20): one multishot accept SQE per listener stays
            // armed across many connections. The kernel re-fires it per
            // incoming conn, each CQE carrying the new fd in `res` and
            // `IORING_CQE_F_MORE` set while still armed. We only re-submit
            // when F_MORE clears (kernel dropped the multishot — listener
            // close, ENOBUFS, etc.). Zero -c1 cost (one persistent conn
            // takes one accept ever); cuts the per-accept SQE under
            // high-conn-churn workloads.
            if !accept_inflight {
                accept_inflight =
                    ring.prep_accept_multishot(self.listener.raw(), OP_ACCEPT);
            }
            if !cl_accept_inflight
                && let Some(cl) = &self.cluster_listener
            {
                cl_accept_inflight =
                    ring.prep_accept_multishot(cl.raw(), OP_ACCEPT_CL);
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
                    OP_RECV => {
                        io_work = true;
                        self.uring_on_recv(cid, c, &mut io, &mut pbuf);
                    }
                    OP_WRITE => {
                        io_work = true;
                        self.uring_on_write(cid, c.res, &mut io);
                        // K4: a write CQE — even a fully-drained one —
                        // wants an arm visit so a chunked-writev tail
                        // (pub/sub burst > IOV_MAX) gets its next
                        // chunk out. Cheap when nothing remains: the
                        // visit's `needs_more` check drops the conn
                        // out of the queue.
                        self.mark_arm_pending(cid, &mut io);
                    }
                    OP_ACCEPT | OP_ACCEPT_CL => {
                        cold_path_hint();
                        let cluster = op == OP_ACCEPT_CL;
                        // B4: only clear the in-flight flag when the
                        // multishot terminates (F_MORE clear). While
                        // F_MORE is set the kernel still has the SQE
                        // armed and will re-fire on the next conn — no
                        // need to re-submit, and the top-of-loop
                        // re-arm gate would queue a duplicate.
                        if !c.has_more() {
                            if cluster {
                                cl_accept_inflight = false;
                            } else {
                                accept_inflight = false;
                            }
                        }
                        io_work = true;
                        if c.res >= 0 {
                            // SAFETY: a freshly accepted fd we now own.
                            let sock = unsafe { Socket::from_raw_fd(c.res) };
                            let _ = sock.set_nodelay();
                            let ncid = self.next_conn_id;
                            self.next_conn_id += 1;
                            let mut conn = Conn::new(sock);
                            conn.cluster = cluster;
                            self.conns.insert(ncid, conn);
                            let mut uc = UringConn::new();
                            // K4: new conn needs an arm visit so its
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
                    _ => {
                        cold_path_hint();
                    }
                }
            }

            // Cross-core: forwarded requests + replies (output accumulates; the
            // io_uring write path below flushes it).
            let did_inbound = self.uring_drain_inbound();
            // K4 (v1.25 A.9): `self.dirty` is no longer cleared here —
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
            // v1.25 A.2: ship the per-shard bio-drop batch to the bio
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
            if let Some(aof) = &mut self.aof {
                let _ = aof.maybe_sync();
            }
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
                if tick_check_counter >= self.tick_check_every || woke_from_park {
                    tick_check_counter = 0;
                    let now = Instant::now();
                    // BLOCK reactor: same cadence as the epoll path so
                    // BLPOP / XREAD BLOCK timeouts fire identically under
                    // either reactor.
                    self.tick_blocked_timeouts();
                    self.tick_xshard_timeouts();
                    if now.duration_since(last_tick) >= iv {
                        self.commands.on_shard_tick(&mut self.store);
                        self.apply_live_runtime_config(&mut tick_interval);
                        self.tick_persist();
                        // v3-cluster replication housekeeping (T1.12.5):
                        // the io_uring path can't watch the replication
                        // listener / replica fds via epoll, so poll them
                        // here once per tick (10 Hz). New replica accepts
                        // see ≤ 100 ms wait; replica handshake bytes ditto.
                        // The streaming pump path stays per-iter via
                        // `pump_replication` (below) — that's where the
                        // throughput-sensitive write side lives.
                        if let Err(e) = self.accept_ready_replication() {
                            eprintln!("kevy: shard {} accept_ready_replication: {e}", self.id);
                        }
                        for idx in 0..self.replicas.len() {
                            if let Err(e) = self.replica_readable(idx) {
                                eprintln!(
                                    "kevy: shard {} replica_readable[{idx}]: {e}",
                                    self.id,
                                );
                            }
                            if let Err(e) = self.replica_writable(idx) {
                                eprintln!(
                                    "kevy: shard {} replica_writable[{idx}]: {e}",
                                    self.id,
                                );
                            }
                        }
                        self.tick_replication_slots(now);
                        self.tick_replication_view();
                        self.tick_replication_watermark();
                        self.drain_replica_inbox();
                        last_tick = now;
                    }
                }
            }

            // Per-iter replication pump (T1.12.5): writes streaming
            // frames + drives snapshot ship chunks. E9: hoist the
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

            // Idle ladder — spin, then park (no nap rung):
            //   1. busy-poll `URING_SPIN_LIMIT` empty iterations, so a -c1
            //      client's next request is reaped immediately;
            //   2. park: io_uring blocking wait, woken by any socket I/O
            //      CQE, the waker pipe, or the bounding timeout. A truly
            //      idle shard costs ~zero CPU.
            //
            // The previous middle rung was a `thread::sleep(200 µs)` nap
            // intended to aggregate inbound work into bigger batches under
            // load. It pinned Rust-client `-c1` throughput at ~4 k ops/s
            // because `thread::sleep` is wake-deaf — a request landing in
            // the nap window paid the full 200 µs regardless of how fast
            // the socket data actually arrived. Both attempted nap
            // replacements (an io_uring `prep_timeout` + `submit_and_wait`
            // variant, and a state-machine refactor) deadlocked under
            // sequential single-conn Rust traffic; removing the nap rung
            // is the simpler, provably-correct fix. Park already wakes
            // instantly on socket CQE, so latency is unaffected; the only
            // cost is the 8-shard high-concurrency throughput note
            // (−18~21 %) that motivated the nap originally, which gets
            // revisited as a v1.22.x follow-up.
            //
            // A non-empty backlog means a peer ring is full — keep
            // spinning to re-attempt the flush (nothing would wake us
            // when the peer drains).
            woke_from_park = false;
            let has_backlog = self.backlog.iter().any(|b| !b.is_empty());
            if !io_work && !did_inbound && !has_backlog {
                idle_spins = idle_spins.saturating_add(1);
                if idle_spins >= URING_SPIN_LIMIT {
                    self.uring_park(&mut ring, &mut park)?;
                    woke_from_park = true;
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
            }
        }
        Ok(())
    }

    // `uring_arm_conns` lives in [`crate::uring_arm`] (v1.25 A.9: this
    // file was 592 LOC pre-K4, over the 500-LOC house rule, and the K4
    // ready-set rewrite needed a clean home). `uring_on_recv` /
    // `uring_mark_closing` / `uring_on_write` live in
    // [`crate::uring_io`]; `uring_drain_inbound` + `uring_reap_closed`
    // live in [`crate::uring_inbox`]; the bounded park lives in
    // [`crate::uring_park`]. All on the same `impl<C: Commands> Shard<C>`
    // and only ever called from `run_uring` above.
    #[doc(hidden)]
    #[allow(dead_code)]
    fn _uring_module_map() {}

    // `uring_on_recv` / `uring_mark_closing` / `uring_on_write` live in
    // [`crate::uring_io`]; `uring_drain_inbound` + `uring_reap_closed`
    // live in [`crate::uring_inbox`]. Same `impl<C: Commands> Shard<C>`,
    // split out so this file stays under the 500-LOC house rule.
}
