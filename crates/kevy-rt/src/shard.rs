//! One shard = one core: the reactor (kqueue/epoll) plus the keyspace it owns.
//!
//! This module is the *transport* half: accepting connections, reading/parsing
//! requests, draining the cross-core inbound rings, and flushing replies in seq
//! order. The command *semantics* (routing, execution, result reduction) live in
//! [`crate::exec`], which adds a second `impl Shard` block. The [`Shard::run`]
//! loop drives socket readiness and the inbound rings until `stop` is set.
//!
//! Cross-core transport is a lock-free SPSC ring per ordered core-pair
//! ([`kevy_ring`]). When a peer's ring is momentarily full, the message spills to
//! a local per-target `backlog`; the loop keeps draining its own inbound and
//! flushing backlogs every iteration, so no shard ever blocks waiting on a peer —
//! that is what keeps the all-to-all mesh deadlock-free.

use crate::Commands;
use crate::blocked::BlockedClients;
use crate::conn::Conn;
use crate::NotificationFlags;
use crate::message::{Inbound, PubMsg, PubSubPatternReg, PubSubReg, ReqBatch};
use kevy_persist::{Aof, load_snapshot, replay_aof};
use kevy_ring::{Consumer, Producer};
use kevy_store::Store;
use kevy_sys::{Event, Poller, Socket, Waker};
use kevy_map::KevyMap;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, fence};

pub(crate) struct Shard<C: Commands> {
    pub(crate) id: usize,
    pub(crate) nshards: usize,
    pub(crate) store: Store,
    pub(crate) commands: C,
    pub(crate) poller: Poller,
    pub(crate) listener: Socket,
    pub(crate) waker: Arc<Waker>,
    /// Inbound SPSC ring from each peer shard (index = source id; `self` = None).
    pub(crate) inboxes: Vec<Option<Consumer<Inbound>>>,
    /// Outbound SPSC ring to each peer shard (index = target id; `self` = None).
    pub(crate) outboxes: Vec<Option<Producer<Inbound>>>,
    /// Per-target overflow queue: messages that didn't fit a full outbound ring,
    /// re-pushed (in order) by `flush_backlog` once the peer drains.
    pub(crate) backlog: Vec<VecDeque<Inbound>>,
    pub(crate) wakers: Vec<Arc<Waker>>,
    // Fx-hashed: these are looked up per command (`conns` twice — start_command
    // + fold) and per event; std's SipHash on the u64/i32 keys profiled at ~17%
    // of single-shard CPU, the dominant non-command-CPU cost.
    pub(crate) conns: KevyMap<u64, Conn>,
    pub(crate) fd_to_conn: KevyMap<i32, u64>,
    pub(crate) next_conn_id: u64,
    pub(crate) events: Vec<Event>,
    pub(crate) read_buf: Vec<u8>,
    /// Targets that received a message this iteration but haven't been woken yet.
    /// Wakeups are coalesced: each target is woken at most once per loop, not
    /// once per message (one pipe-write syscall instead of N).
    pub(crate) pending_wakes: Vec<bool>,
    /// Per-shard "is this core parked (blocking) right now?" flags. A sender only
    /// needs a syscall wakeup for a parked peer; a spinning peer sees the message
    /// on its next poll. Indexed by shard id; `parked[self.id]` is our own.
    pub(crate) parked: Vec<Arc<AtomicBool>>,
    pub(crate) data_dir: PathBuf,
    /// `None` disables the append-only log (e.g. pure in-memory benchmarking).
    pub(crate) aof: Option<Aof>,
    /// `auto_aof_rewrite_percentage`: trigger BGREWRITEAOF when the live
    /// AOF is at least this percent larger than at the previous rewrite.
    /// `0` disables auto-rewrite.
    pub(crate) auto_aof_rewrite_pct: u32,
    /// `auto_aof_rewrite_min_size`: never auto-rewrite an AOF smaller than
    /// this many bytes (prevents thrash during startup / on tiny data).
    pub(crate) auto_aof_rewrite_min_size: u64,
    /// Connections a PUBLISH appended output to this iteration; the reactor
    /// flushes them (epoll via `flush_conn`, io_uring via its arm/write loop).
    pub(crate) dirty: Vec<u64>,
    /// Shared pub/sub channel registry (see [`PubSubReg`]).
    pub(crate) pubsub: PubSubReg,
    /// Shared pub/sub pattern registry (see [`PubSubPatternReg`]).
    /// Empty in steady state; PUBLISH skips the walk when so.
    pub(crate) pubsub_patterns: PubSubPatternReg,
    /// This shard's local pattern → conn ids table. Mirrors `pubsub`'s
    /// channel-table role for the channel-precise path. Each
    /// `PSUBSCRIBE` adds an entry; each delivered `PUBLISH` runs
    /// `glob_match` against every key (only when the map is non-empty —
    /// the steady-state O(1) `is_empty()` short-circuit keeps the
    /// channel-only PUBLISH hot path untouched).
    pub(crate) psub_local: HashMap<Vec<u8>, Vec<u64>>,
    /// Per-target-shard accumulated pub/sub deliveries, flushed once per loop
    /// (`flush_publish`) so a PUBLISH flood batches into one send per shard.
    pub(crate) publish_batch: Vec<Vec<PubMsg>>,
    /// Per-owning-shard accumulated single-key dispatches, flushed once per loop
    /// (`flush_requests`) so a -c50 flood costs one cross-core send per shard,
    /// not one per command — amortizing the ring/fold tax that drags many
    /// shards below single-shard throughput.
    pub(crate) request_batch: Vec<ReqBatch>,
    /// Per-shard cached `notify_keyspace_events` flags — hot-reloaded
    /// off the [`crate::Commands::live_runtime_config`] tick. Empty
    /// (default) = OFF: every write checks `notify_flags.is_empty()`
    /// and skips the publish hot-path. `Copy` so the per-cmd check
    /// fits in a register pair.
    pub(crate) notify_flags: NotificationFlags,
    /// Iterations the reactor busy-polls before parking. Threaded in
    /// from [`crate::Runtime::with_advanced`]; replaces the old
    /// `SPIN_LIMIT` const so embedders can tune wake-up vs idle CPU.
    pub(crate) spin_limit: u32,
    /// Bounded blocking-wait timeout (ms) when parked. Acts as a
    /// safety backstop against missed cross-core wakes. Replaces the
    /// old `PARK_TIMEOUT_MS` const.
    pub(crate) park_timeout_ms: i32,
    /// Reactor loop iterations between wall-clock reads for the tick
    /// check. Replaces the old `TICK_CHECK_EVERY` const.
    pub(crate) tick_check_every: u32,
    /// SLOWLOG ring + threshold (see [`crate::exec_slowlog::SlowlogState`]).
    /// Hot-reload via `apply_live_runtime_config` when the embedder
    /// returns `Some` in `LiveRuntimeConfig::slowlog_*`.
    pub(crate) slowlog: crate::exec_slowlog::SlowlogState,
    /// Per-shard blocked-client registry (see [`crate::blocked`]). The
    /// in-shard fast path for a single key on this shard: `BLPOP` /
    /// `BRPOP` / `XREAD BLOCK` / `XREADGROUP BLOCK`. Empty in steady
    /// state, so the wake / tick hot paths short-circuit on `is_empty()`.
    pub(crate) blocked: BlockedClients,
    /// Origin-side records for conns blocked across shards (a single
    /// remote key or any multi-key form). This shard is the arbiter for
    /// each. Empty in steady state. See [`crate::block_xshard`].
    pub(crate) origin_blocks: HashMap<u64, crate::block_xshard::OriginBlock>,
    /// Target-side cross-shard waiters: (possibly remote) conns blocked on
    /// keys this shard owns. Kept separate from `blocked` so the hot
    /// single-key-local path is untouched. Empty in steady state.
    pub(crate) xwaiters: crate::block_xshard::XShardWaiters,
}

// `SPIN_LIMIT` / `PARK_TIMEOUT_MS` / `TICK_CHECK_EVERY` moved to per-
// shard fields (`Shard.spin_limit` / `park_timeout_ms` /
// `tick_check_every`) since workspace v1.4 — wired through
// `[advanced]` config + `Runtime::with_advanced`. Defaults
// (`256` / `50ms` / `256`) match the pre-v1.4 hardcoded values, so
// existing benchmark numbers translate one-to-one.

impl<C: Commands> Shard<C> {
    /// This shard's snapshot file: `<data_dir>/dump-<id>.rdb`.
    pub(crate) fn snapshot_path(&self) -> PathBuf {
        self.data_dir.join(format!("dump-{}.rdb", self.id))
    }

    /// This shard's append-only log: `<data_dir>/aof-<id>.aof`.
    pub(crate) fn aof_path(&self) -> PathBuf {
        self.data_dir.join(format!("aof-{}.aof", self.id))
    }

    pub(crate) fn run(mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
        // Restore: snapshot (state as of last SAVE) then replay the AOF (writes
        // since that SAVE). The AOF is truncated at each SAVE, so this never
        // double-applies. Replay goes straight to the store (no re-logging).
        let snap = self.snapshot_path();
        if snap.exists()
            && let Err(e) = load_snapshot(&mut self.store, &snap)
        {
            eprintln!(
                "kevy: shard {} failed to load {}: {e}",
                self.id,
                snap.display()
            );
        }
        if self.aof.is_some() {
            let aof_path = self.aof_path();
            let commands = &self.commands;
            let store = &mut self.store;
            replay_aof(&aof_path, |args| {
                commands.dispatch(store, &args);
            })?;
        }

        self.listener.set_nonblocking()?;
        self.poller.add(self.listener.raw(), true, false)?;
        self.poller.add(self.waker.read_fd(), true, false)?;
        let listener_fd = self.listener.raw();
        let waker_fd = self.waker.read_fd();
        let me = self.id;

        let mut tick_interval = match self.commands.shard_tick_interval_ms() {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        };
        let mut last_tick = Instant::now();
        let mut tick_check_counter: u32 = 0;

        let mut idle_spins: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            // Busy-poll while there's recent work — a cross-core hop then costs
            // no syscall. Park (blocking wait) once we've been idle a while.
            let spinning = idle_spins < self.spin_limit;
            let timeout = if spinning {
                Some(0)
            } else {
                self.parked[me].store(true, Ordering::SeqCst);
                // Close the park/wake race: the SeqCst fence pairs with
                // the matching fence in `flush_wakes` on every other
                // shard, so any push that lands BEFORE this drain on the
                // peer's side is either (a) seen by `drain_inbound` here
                // OR (b) the peer's parked-load saw `true` and a wake
                // syscall is on the way. Without the fence, the lost-wake
                // window was bounded by `PARK_TIMEOUT_MS` (50 ms) — the
                // blocking wait below is now defense-in-depth (covers a
                // missed eventfd write, OS scheduling glitch, etc.).
                // Loom-verified by `tests/loom.rs::park_wake_fence_*`.
                fence(Ordering::SeqCst);
                if self.drain_inbound()? {
                    self.parked[me].store(false, Ordering::SeqCst);
                    self.flush_backlog();
                    self.flush_dirty()?;
                    self.flush_wakes();
                    idle_spins = 0;
                    continue;
                }
                Some(self.park_timeout_ms)
            };

            self.poller.wait(&mut self.events, timeout)?;
            if !spinning {
                self.parked[me].store(false, Ordering::SeqCst);
            }

            let mut did_work = !self.events.is_empty();
            if did_work {
                // mem::take only when there's actually work, avoids two Vec
                // moves per empty iter (timeout=Some(0) often returns 0).
                let events = std::mem::take(&mut self.events);
                for ev in &events {
                    if ev.fd == listener_fd {
                        self.accept_ready()?;
                    } else if ev.fd == waker_fd {
                        self.waker.drain();
                    } else if let Some(&conn_id) = self.fd_to_conn.get(&ev.fd) {
                        if ev.readable || ev.hup {
                            self.conn_readable(conn_id)?;
                        } else if ev.writable {
                            self.flush_conn(conn_id)?;
                        }
                    }
                }
                self.events = events;
            }

            // Messages from other cores (forwarded requests + replies to ours).
            if self.drain_inbound()? {
                did_work = true;
            }
            // Re-push anything that overflowed a full ring last iteration.
            self.flush_backlog();
            // Send this iteration's batched single-key dispatches (one per target).
            self.flush_requests();
            // Send this iteration's batched pub/sub deliveries (one per target).
            self.flush_publish();
            // Flush subscribers a PUBLISH wrote to this iteration.
            self.flush_dirty()?;
            // One wakeup per touched (and parked) target this iteration.
            self.flush_wakes();
            // Honor the EverySec AOF fsync window.
            if let Some(aof) = &mut self.aof {
                let _ = aof.maybe_sync();
            }
            // Active TTL reaper / shard housekeeping. Skip the wall-clock
            // read on most iters: in busy-poll the tick fires at 10 Hz
            // with negligible overhead (counter saturates in ~us, then
            // checks elapsed). In park mode each iter is already ≥ 1 ms
            // so the throttle would delay the tick by 256 iters × 50 ms
            // = ~12 s on a fully-idle server — bypass the counter when
            // we just came back from a parking wait so the tick fires
            // at every park iteration regardless of recent traffic.
            if let Some(iv) = tick_interval {
                tick_check_counter = tick_check_counter.wrapping_add(1);
                if tick_check_counter >= self.tick_check_every || !spinning {
                    tick_check_counter = 0;
                    let now = Instant::now();
                    // BLOCK reactor: fire timeouts every tick gate (not gated
                    // by `iv`), so a `BLPOP k 0.5` resolves on the next 50ms
                    // park instead of the next user-level shard tick.
                    self.tick_blocked_timeouts();
                    self.tick_xshard_timeouts();
                    if now.duration_since(last_tick) >= iv {
                        self.commands.on_shard_tick(&mut self.store);
                        self.apply_live_runtime_config(&mut tick_interval);
                        self.maybe_auto_rewrite_aof();
                        last_tick = now;
                    }
                }
            }

            // A non-empty backlog means a peer ring is full: keep spinning so we
            // re-attempt the flush (and keep draining inbound to unblock peers).
            let has_backlog = self.backlog.iter().any(|b| !b.is_empty());
            idle_spins = if did_work || has_backlog {
                0
            } else {
                idle_spins.saturating_add(1)
            };
        }
        Ok(())
    }

    // `apply_live_runtime_config` + `maybe_auto_rewrite_aof` (the
    // per-tick housekeeping) live in [`crate::shard_tick`] — same
    // `impl<C: Commands> Shard<C>`, split out so this file stays under
    // the 500-LOC house rule.

    /// Wake every target enqueued to this iteration that is currently parked.
    /// A spinning peer needs no syscall — it will see the message on its next
    /// poll(0). This is what removes the per-message wakeup under load.
    pub(crate) fn flush_wakes(&mut self) {
        // Fast-path single-shard: pending_wakes is len-nshards; in the common
        // single-shard benchmark this loop runs nshards times even when no
        // wakes are pending. Skip outright when nothing's flagged.
        if !self.pending_wakes.iter().any(|&w| w) {
            return;
        }
        // Close the park/wake race: the SeqCst fence pairs with the
        // matching fence in `Shard::run` after a peer stores `parked=true`.
        // Combined, they guarantee: if our ring push (Release on the
        // outbox's tail, executed earlier this iteration via `send_to`)
        // happens-before this load, AND the peer's parked-store
        // happens-before its post-park drain, then either
        //   (a) the peer's drain sees our push,            OR
        //   (b) our load sees `parked=true` and we send the wake.
        // Loom-verified by `kevy-rt/tests/loom.rs::no_wake_implies_drained`.
        // Without the fence the lost-wake window was bounded by the
        // peer's `PARK_TIMEOUT_MS` (50 ms); the timeout remains as
        // defense-in-depth against missed eventfd writes / OS hiccups.
        fence(Ordering::SeqCst);
        for i in 0..self.pending_wakes.len() {
            if self.pending_wakes[i] {
                self.pending_wakes[i] = false;
                if self.parked[i].load(Ordering::SeqCst) {
                    let _ = self.wakers[i].wake();
                }
            }
        }
    }

    /// Flush connections a PUBLISH appended output to this iteration (epoll path;
    /// the io_uring reactor flushes them via its arm/write loop instead).
    #[inline]
    fn flush_dirty(&mut self) -> io::Result<()> {
        // `appendfsync always` loop-level group commit: one fsync for the
        // whole iteration's buffered writes BEFORE any deferred reply leaves
        // (durable-before-reply, one fsync per loop instead of per command).
        // No-op in every other mode.
        self.aof_end_group()?;
        if self.dirty.is_empty() {
            return Ok(());
        }
        while let Some(id) = self.dirty.pop() {
            self.flush_conn(id)?;
        }
        Ok(())
    }

    fn accept_ready(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok(sock) => {
                    sock.set_nonblocking()?;
                    let _ = sock.set_nodelay();
                    let fd = sock.raw();
                    let id = self.next_conn_id;
                    self.next_conn_id += 1;
                    self.poller.add(fd, true, false)?;
                    self.fd_to_conn.insert(fd, id);
                    self.conns.insert(id, Conn::new(sock));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        Ok(())
    }

    // `conn_readable` (socket read + parse + dispatch) lives in
    // [`crate::inbox`] alongside `drain_inbound` + `close_conn` — all the
    // event handlers the `run` loop dispatches to. Still the same
    // `impl Shard`.

    /// Enqueue a message to another shard, marking it for a coalesced wakeup. The
    /// fast path is a lock-free ring push; on a full ring it spills to the local
    /// per-target backlog (preserving order), which `flush_backlog` drains later.
    pub(crate) fn send_to(&mut self, dst: usize, msg: Inbound) {
        if self.backlog[dst].is_empty() {
            match self.outboxes[dst].as_mut() {
                Some(p) => {
                    if let Err(m) = p.push(msg) {
                        self.backlog[dst].push_back(m);
                    }
                }
                // `dst == self.id` has no ring and is never sent to.
                None => return,
            }
        } else {
            // Order: queue behind the existing backlog rather than jumping the ring.
            self.backlog[dst].push_back(msg);
        }
        self.pending_wakes[dst] = true;
    }

    /// Re-push each per-target backlog into its ring (filled when a ring was full
    /// last iteration). Stops at the first target whose ring is still full.
    #[inline]
    pub(crate) fn flush_backlog(&mut self) {
        // Outer-empty short-circuit: in the hot single-shard / no-backlog
        // path this avoids the nshards loop entirely.
        if self.backlog.iter().all(|b| b.is_empty()) {
            return;
        }
        for dst in 0..self.nshards {
            if self.backlog[dst].is_empty() {
                continue;
            }
            let Some(p) = self.outboxes[dst].as_mut() else {
                self.backlog[dst].clear();
                continue;
            };
            while let Some(msg) = self.backlog[dst].pop_front() {
                if let Err(m) = p.push(msg) {
                    self.backlog[dst].push_front(m);
                    break;
                }
                self.pending_wakes[dst] = true;
            }
        }
    }

    // `drain_inbound` + `close_conn` live in [`crate::inbox`] to keep this
    // file under the 500-LOC house rule; they're still on the same
    // `impl Shard` and called from `run()` here.

    pub(crate) fn flush_conn(&mut self, conn_id: u64) -> io::Result<()> {
        let (close, want_write, fd) = {
            let Some(conn) = self.conns.get_mut(&conn_id) else {
                return Ok(());
            };
            while conn.write_pos < conn.output.len() {
                match conn.sock.write(&conn.output[conn.write_pos..]) {
                    Ok(0) => break,
                    Ok(n) => conn.write_pos += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        conn.closing = true;
                        break;
                    }
                }
            }
            if conn.write_pos == conn.output.len() {
                conn.output.clear();
                conn.write_pos = 0;
            }
            let out_remaining = conn.write_pos < conn.output.len();
            let close = conn.closing && conn.pending.is_empty() && !out_remaining;
            (close, out_remaining, conn.sock.raw())
        };

        if close {
            self.close_conn(conn_id);
            return Ok(());
        }
        if let Some(conn) = self.conns.get_mut(&conn_id)
            && want_write != conn.want_write
        {
            conn.want_write = want_write;
            self.poller.modify(fd, true, want_write)?;
        }
        Ok(())
    }

    /// Drop a (closing) connection's subscriptions from the shared registry, so
    /// PUBLISH counts and the fan-out bitset don't count a gone subscriber.
    pub(crate) fn unregister_subs(&self, subs: &std::collections::HashSet<Vec<u8>>) {
        if subs.is_empty() {
            return;
        }
        let mut reg = self.pubsub.write().expect("pubsub registry");
        for ch in subs {
            let drop = match reg.get_mut(ch) {
                Some(e) => {
                    e.0 = e.0.saturating_sub(1);
                    e.0 == 0
                }
                None => false,
            };
            if drop {
                reg.remove(ch);
            }
        }
    }
}
