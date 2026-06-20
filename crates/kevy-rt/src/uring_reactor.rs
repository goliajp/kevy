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
use kevy_uring::{Completion, IoUring, ProvidedBufRing};
use kevy_map::KevyMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// SQ/CQ depth for the per-shard ring.
const URING_ENTRIES: u32 = 256;
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
/// Shared provided-buffer ring per shard: `PBUF_ENTRIES` buffers of `PBUF_SIZE`
/// bytes feed the multishot recvs of every connection. One recv may fill a whole
/// buffer; larger arrivals span several (reassembled in `Conn::input`). 128 × 16K
/// = 2 MiB/shard, recycled immediately after each completion is drained.
const PBUF_ENTRIES: u16 = 128;
const PBUF_SIZE: u32 = 16 * 1024;
const PBUF_GROUP: u16 = 0;
/// `-ENOBUFS`: the buf ring was momentarily empty; just re-arm (don't close).
const ENOBUFS: i32 = 105;

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
const OP_RECV: u64 = 1 << OP_SHIFT;
const OP_WRITE: u64 = 2 << OP_SHIFT;
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
            // Always keep one accept in flight (per listener).
            if !accept_inflight {
                accept_inflight = ring.prep_accept(self.listener.raw(), OP_ACCEPT);
            }
            if !cl_accept_inflight
                && let Some(cl) = &self.cluster_listener
            {
                cl_accept_inflight = ring.prep_accept(cl.raw(), OP_ACCEPT_CL);
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
            for c in &comps {
                let op = c.user_data & !CONN_MASK;
                let cid = c.user_data & CONN_MASK;
                match op {
                    OP_ACCEPT | OP_ACCEPT_CL => {
                        let cluster = op == OP_ACCEPT_CL;
                        if cluster {
                            cl_accept_inflight = false;
                        } else {
                            accept_inflight = false;
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
                            io.insert(ncid, UringConn::new());
                            // Client connections only — cluster-bus is internal.
                            if !cluster {
                                self.commands.on_connection();
                            }
                        }
                    }
                    OP_RECV => {
                        io_work = true;
                        self.uring_on_recv(cid, c, &mut io, &mut pbuf);
                    }
                    OP_WRITE => {
                        io_work = true;
                        self.uring_on_write(cid, c.res, &mut io);
                    }
                    OP_WAKER => {
                        park.waker_armed = false;
                        // The read took ≤ 8 bytes; clear any pile-up beyond it.
                        self.waker.drain();
                    }
                    OP_TIMEOUT => park.timeout_inflight = false,
                    _ => {}
                }
            }

            // Cross-core: forwarded requests + replies (output accumulates; the
            // io_uring write path below flushes it).
            let did_inbound = self.uring_drain_inbound();
            // PUBLISH appended to subscribers' output + marked them dirty; the
            // arm loop above already submits a write for any conn with output, so
            // io_uring batches the delivery — just drop the (epoll-only) marks.
            self.dirty.clear();
            self.flush_backlog();
            self.flush_requests();
            self.flush_publish();
            self.flush_wakes();
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
                }
            } else {
                idle_spins = 0;
            }
        }
        Ok(())
    }

    /// Submit a read for every idle open conn and a write for every conn with
    /// pending output, reusing one fixed buffer per direction per conn.
    ///
    /// One pass over `conns` with one `io` probe per conn: this loop runs
    /// every reactor iteration, and the previous shape (a `keys()` snapshot
    /// Vec + 3-8 map probes per conn to appease the borrow checker) was the
    /// hottest block of `run_uring` self time on the 8-shard profile. `conns`
    /// and `io` are disjoint borrows (`io` lives on `run_uring`'s stack), so
    /// `iter_mut` needs no snapshot — nothing here inserts or removes.
    fn uring_arm_conns(
        &mut self,
        ring: &mut IoUring,
        io: &mut KevyMap<u64, UringConn>,
        bgid: u16,
    ) {
        for (&cid, conn) in self.conns.iter_mut() {
            let Some(uc) = io.get_mut(&cid) else {
                continue;
            };
            // Start a new write: move the conn's output into the stable write_buf.
            if !uc.write_inflight && uc.write_buf.is_empty() && !conn.output.is_empty() {
                std::mem::swap(&mut uc.write_buf, &mut conn.output);
                uc.write_off = 0;
            }
            // Submit the write (fresh or a partial-write continuation).
            if !uc.write_inflight && uc.write_off < uc.write_buf.len() {
                // SAFETY: write_buf is owned, stable, and outlives the SQE.
                let ok = unsafe {
                    ring.prep_write(
                        conn.sock.raw(),
                        uc.write_buf.as_ptr().add(uc.write_off),
                        (uc.write_buf.len() - uc.write_off) as u32,
                        OP_WRITE | cid,
                    )
                };
                if ok {
                    uc.write_inflight = true;
                }
            }
            // Arm a multishot recv if one isn't already running (it re-fires per
            // arrival into the shared provided-buffer ring, so this happens once
            // per connection, not once per read — the syscall-batching win).
            if !uc.recv_armed
                && !uc.closing
                && ring.prep_recv_multishot(conn.sock.raw(), bgid, OP_RECV | cid)
            {
                uc.recv_armed = true;
            }
        }
    }

    /// A multishot recv completed: copy the kernel-picked buffer's bytes into the
    /// conn, recycle it, run every complete command, and re-arm if the SQE ended.
    fn uring_on_recv(
        &mut self,
        cid: u64,
        c: &Completion,
        io: &mut KevyMap<u64, UringConn>,
        pbuf: &mut ProvidedBufRing,
    ) {
        // The multishot SQE stops firing once a completion lacks F_MORE (error,
        // ENOBUFS, or EOF) — mark it for re-arming next loop.
        if !c.has_more()
            && let Some(uc) = io.get_mut(&cid)
        {
            uc.recv_armed = false;
        }
        if c.res <= 0 {
            // Close on EOF (0) or a real error, but NOT on -ENOBUFS (the ring was
            // momentarily empty; the data is still queued, so just re-arm).
            if c.res != -ENOBUFS {
                self.uring_mark_closing(cid, io);
            }
            return;
        }
        // res > 0: a buffer was filled; copy it out and return it to the ring.
        // (A zero-copy parse straight from the provided buffer was measured
        // flat — the copy is cheap next to dispatch — so the single
        // append-then-parse shape stays.)
        let Some(bid) = c.buffer_id() else {
            return; // no buffer (shouldn't happen for a successful recv)
        };
        let n = c.res as usize;
        if let Some(conn) = self.conns.get_mut(&cid) {
            conn.input.extend_from_slice(pbuf.bytes(bid, n));
        }
        pbuf.recycle(bid);
        // Swap `conn.input` onto the stack so the borrowed argvs don't
        // collide with `&mut self` in dispatch; one tail drain at the end,
        // then the buf swaps back (if the conn still exists).
        let mut input_buf = match self.conns.get_mut(&cid) {
            Some(c) => std::mem::take(&mut c.input),
            None => return,
        };
        // AOF group-commit window (mirrors the epoll `conn_readable` path):
        // `appendfsync always` buffers this batch's writes and fsyncs once in
        // `aof_end_group`, which runs before the io_uring write loop submits
        // the replies — so durability still precedes reply.
        self.aof_begin_group();
        let outcome = self.dispatch_batch(cid, &input_buf);
        self.aof_end_group_logged();
        if !outcome.conn_gone {
            input_buf.drain(..outcome.consumed);
            if let Some(c) = self.conns.get_mut(&cid) {
                c.input = input_buf;
            }
        }
        if outcome.conn_gone {
            return;
        }
        if outcome.protocol_error {
            self.protocol_error(cid);
            self.uring_mark_closing(cid, io);
        }
    }

    /// Mark `cid` closing and eagerly cancel its block waiters (local
    /// parked BLPOP/XREAD + cross-shard arbiter registrations). The full
    /// teardown still happens in `uring_reap_closed`, but that runs on a
    /// 1/16-iteration throttle — without the eager cancel a dead conn's
    /// waiter stayed live for up to 16 iterations and could consume a
    /// push (e.g. an LPUSH element) meant for a live client.
    fn uring_mark_closing(&mut self, cid: u64, io: &mut KevyMap<u64, UringConn>) {
        if let Some(uc) = io.get_mut(&cid) {
            uc.closing = true;
        }
        self.blocked.drop_for_conn(cid);
        self.cancel_xshard_on_close(cid);
    }

    /// A write completed: advance progress; resubmit the remainder next loop.
    fn uring_on_write(&mut self, cid: u64, res: i32, io: &mut KevyMap<u64, UringConn>) {
        let Some(uc) = io.get_mut(&cid) else {
            return;
        };
        uc.write_inflight = false;
        if res < 0 {
            self.uring_mark_closing(cid, io);
            return;
        }
        uc.write_off += res as usize;
        if uc.write_off >= uc.write_buf.len() {
            uc.write_buf.clear();
            uc.write_off = 0;
        }
    }

    // `uring_drain_inbound` + `uring_reap_closed` (the cross-core drain
    // and connection-reap half) live in [`crate::uring_inbox`] — same
    // `impl<C: Commands> Shard<C>`, split out so this file stays under
    // the 500-LOC house rule.
}
