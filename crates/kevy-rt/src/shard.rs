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
use crate::conn::Conn;
use crate::message::{Inbound, Op, PubMsg, PubSubReg, ReqBatch};
use kevy_persist::{Aof, load_snapshot, replay_aof};
use kevy_resp::parse_command;
use kevy_ring::{Consumer, Producer};
use kevy_store::Store;
use kevy_sys::{Event, Poller, Socket, Waker};
use kevy_hash::FxHashMap;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    pub(crate) conns: FxHashMap<u64, Conn>,
    pub(crate) fd_to_conn: FxHashMap<i32, u64>,
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
    /// Connections a PUBLISH appended output to this iteration; the reactor
    /// flushes them (epoll via `flush_conn`, io_uring via its arm/write loop).
    pub(crate) dirty: Vec<u64>,
    /// Shared pub/sub channel registry (see [`PubSubReg`]).
    pub(crate) pubsub: PubSubReg,
    /// Per-target-shard accumulated pub/sub deliveries, flushed once per loop
    /// (`flush_publish`) so a PUBLISH flood batches into one send per shard.
    pub(crate) publish_batch: Vec<Vec<PubMsg>>,
    /// Per-owning-shard accumulated single-key dispatches, flushed once per loop
    /// (`flush_requests`) so a -c50 flood costs one cross-core send per shard,
    /// not one per command — amortizing the ring/fold tax that drags many
    /// shards below single-shard throughput.
    pub(crate) request_batch: Vec<ReqBatch>,
}

/// Iterations to busy-poll (timeout 0) after the last work before parking.
const SPIN_LIMIT: u32 = 256;
/// Backstop blocking-wait timeout when parked. Socket/cross-core readiness wakes
/// us sooner; this only bounds latency if a wakeup is ever missed.
const PARK_TIMEOUT_MS: i32 = 50;

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

        let mut idle_spins: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            // Busy-poll while there's recent work — a cross-core hop then costs
            // no syscall. Park (blocking wait) once we've been idle a while.
            let spinning = idle_spins < SPIN_LIMIT;
            let timeout = if spinning {
                Some(0)
            } else {
                self.parked[me].store(true, Ordering::SeqCst);
                // Close the park/wake race: drain once more after advertising
                // "parked"; the blocking wait is also a backstop against a miss.
                if self.drain_inbound()? {
                    self.parked[me].store(false, Ordering::SeqCst);
                    self.flush_backlog();
                    self.flush_dirty()?;
                    self.flush_wakes();
                    idle_spins = 0;
                    continue;
                }
                Some(PARK_TIMEOUT_MS)
            };

            self.poller.wait(&mut self.events, timeout)?;
            if !spinning {
                self.parked[me].store(false, Ordering::SeqCst);
            }

            let events = std::mem::take(&mut self.events);
            let mut did_work = !events.is_empty();
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

    /// Wake every target enqueued to this iteration that is currently parked.
    /// A spinning peer needs no syscall — it will see the message on its next
    /// poll(0). This is what removes the per-message wakeup under load.
    pub(crate) fn flush_wakes(&mut self) {
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
    fn flush_dirty(&mut self) -> io::Result<()> {
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

    fn conn_readable(&mut self, conn_id: u64) -> io::Result<()> {
        {
            let Some(conn) = self.conns.get_mut(&conn_id) else {
                return Ok(());
            };
            loop {
                match conn.sock.read(&mut self.read_buf) {
                    Ok(0) => {
                        conn.closing = true;
                        break;
                    }
                    Ok(n) => conn.input.extend_from_slice(&self.read_buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        conn.closing = true;
                        break;
                    }
                }
            }
        }

        loop {
            let parsed = {
                let Some(conn) = self.conns.get_mut(&conn_id) else {
                    return Ok(());
                };
                match parse_command(&conn.input) {
                    Ok(Some((args, consumed))) => {
                        conn.input.drain(..consumed);
                        Some(Ok(args))
                    }
                    Ok(None) => None,
                    Err(_) => Some(Err(())),
                }
            };
            match parsed {
                Some(Ok(args)) => self.handle_command(conn_id, args),
                Some(Err(())) => {
                    self.protocol_error(conn_id);
                    break;
                }
                None => break,
            }
        }
        self.flush_conn(conn_id)
    }

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
    pub(crate) fn flush_backlog(&mut self) {
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

    /// Drain inbound cross-core messages from every peer ring; returns whether
    /// any were processed.
    fn drain_inbound(&mut self) -> io::Result<bool> {
        let mut did = false;
        for src in 0..self.nshards {
            if src == self.id {
                continue; // no self-ring
            }
            while let Some(msg) = self.inboxes[src].as_mut().expect("peer inbox").pop() {
                did = true;
                match msg {
                    Inbound::Request {
                        origin,
                        conn,
                        seq,
                        op,
                    } => {
                        let part = self.exec_op(op);
                        self.send_to(origin, Inbound::Response { conn, seq, part });
                    }
                    Inbound::Response { conn, seq, part } => {
                        self.fold(conn, seq, part);
                        self.flush_conn(conn)?;
                    }
                    // Batched single-key dispatches to this (owning) shard: exec
                    // each locally, reply as one `ResponseBatch` to the origin.
                    Inbound::RequestBatch { origin, reqs } => {
                        let mut resps = Vec::with_capacity(reqs.len());
                        for (conn, seq, argv) in reqs {
                            let part = self.exec_op(Op::Dispatch(argv));
                            resps.push((conn, seq, part));
                        }
                        self.send_to(origin, Inbound::ResponseBatch(resps));
                    }
                    // Batched replies: fold each by seq, then flush each touched
                    // conn once (dedup — pipelined replies share a conn).
                    Inbound::ResponseBatch(resps) => {
                        let mut to_flush: Vec<u64> = Vec::new();
                        for (conn, seq, part) in resps {
                            self.fold(conn, seq, part);
                            if !to_flush.contains(&conn) {
                                to_flush.push(conn);
                            }
                        }
                        for conn in to_flush {
                            self.flush_conn(conn)?;
                        }
                    }
                    // Fire-and-forget batched pub/sub delivery; appended
                    // subscriber output is flushed via `flush_dirty`.
                    Inbound::DeliverPublish(batch) => {
                        for m in &batch {
                            self.deliver_publish(&m.0, &m.1);
                        }
                    }
                }
            }
        }
        Ok(did)
    }

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

    fn close_conn(&mut self, conn_id: u64) {
        if let Some(conn) = self.conns.remove(&conn_id) {
            let fd = conn.sock.raw();
            let _ = self.poller.delete(fd);
            self.fd_to_conn.remove(&fd);
            self.unregister_subs(&conn.sub);
            // conn (and its Socket) dropped here → fd closed.
        }
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
