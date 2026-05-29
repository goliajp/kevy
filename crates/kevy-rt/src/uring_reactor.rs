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
//! Step-1 scope: accept + per-conn read → dispatch → write, plus the cross-core
//! drain, which is enough for the full `sharded` suite. It busy-polls and sleeps
//! briefly when idle (an `IORING_OP_TIMEOUT` park is a follow-up). Pub/sub's
//! direct `flush_conn` write is not yet wired here (no pub/sub in `sharded`).

use crate::Commands;
use crate::conn::Conn;
use crate::message::{Inbound, Op};
use crate::shard::Shard;
use kevy_persist::{load_snapshot, replay_aof};
use kevy_resp::parse_command_into;
use kevy_sys::Socket;
use kevy_uring::{Completion, IoUring, ProvidedBufRing};
use kevy_map::KevyMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// SQ/CQ depth for the per-shard ring.
const URING_ENTRIES: u32 = 256;
/// Busy-poll iterations after the last work before yielding the core (mirrors
/// the epoll reactor's `SPIN_LIMIT`). Keeps -c1 latency low without spinning a
/// quiet shard at 100% forever.
const URING_SPIN_LIMIT: u32 = 256;
/// Shared provided-buffer ring per shard: `PBUF_ENTRIES` buffers of `PBUF_SIZE`
/// bytes feed the multishot recvs of every connection. One recv may fill a whole
/// buffer; larger arrivals span several (reassembled in `Conn::input`). 128 × 16K
/// = 2 MiB/shard, recycled immediately after each completion is drained.
const PBUF_ENTRIES: u16 = 128;
const PBUF_SIZE: u32 = 16 * 1024;
const PBUF_GROUP: u16 = 0;
/// `-ENOBUFS`: the buf ring was momentarily empty; just re-arm (don't close).
const ENOBUFS: i32 = 105;

// `user_data` layout: top 2 bits = op, low 62 bits = conn id.
const OP_SHIFT: u32 = 62;
const OP_RECV: u64 = 1 << OP_SHIFT;
const OP_WRITE: u64 = 2 << OP_SHIFT;
const OP_ACCEPT: u64 = 3 << OP_SHIFT;
const CONN_MASK: u64 = (1 << OP_SHIFT) - 1;

/// io_uring-specific per-connection state (the byte buffers that must outlive
/// their in-flight SQEs). The command-level state stays in the shard's [`Conn`].
struct UringConn {
    /// A multishot recv SQE is armed for this conn (re-fires per arrival, drawing
    /// from the shard's provided-buffer ring). Re-armed only when it terminates.
    recv_armed: bool,
    /// Stable buffer for an in-flight write (swapped in from `Conn::output`).
    write_buf: Vec<u8>,
    write_off: usize,
    write_inflight: bool,
    /// EOF/error seen on the socket — close once writes drain.
    closing: bool,
}

impl UringConn {
    fn new() -> Self {
        UringConn {
            recv_armed: false,
            write_buf: Vec::new(),
            write_off: 0,
            write_inflight: false,
            closing: false,
        }
    }
}

impl<C: Commands> Shard<C> {
    /// Completion-based run loop (Linux io_uring). Mirrors [`Shard::run`] but
    /// drives socket I/O through io_uring instead of the readiness poller.
    pub(crate) fn run_uring(mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
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
        let mut comps: Vec<Completion> = Vec::with_capacity(URING_ENTRIES as usize);
        let mut cids: Vec<u64> = Vec::new();
        let mut idle_spins: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            // Always keep one accept in flight.
            if !accept_inflight {
                accept_inflight = ring.prep_accept(self.listener.raw(), OP_ACCEPT);
            }
            self.uring_arm_conns(&mut ring, &mut io, &mut cids, pbuf.group());

            ring.submit_and_wait(0)?; // submit queued SQEs; reap is non-blocking
            comps.clear();
            ring.for_each_completion(|c| comps.push(c));

            for c in &comps {
                let op = c.user_data & !CONN_MASK;
                let cid = c.user_data & CONN_MASK;
                match op {
                    OP_ACCEPT => {
                        accept_inflight = false;
                        if c.res >= 0 {
                            // SAFETY: a freshly accepted fd we now own.
                            let sock = unsafe { Socket::from_raw_fd(c.res) };
                            let _ = sock.set_nodelay();
                            let ncid = self.next_conn_id;
                            self.next_conn_id += 1;
                            self.conns.insert(ncid, Conn::new(sock));
                            io.insert(ncid, UringConn::new());
                        }
                    }
                    OP_RECV => self.uring_on_recv(cid, c, &mut io, &mut pbuf),
                    OP_WRITE => self.uring_on_write(cid, c.res, &mut io),
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
            self.uring_reap_closed(&mut io);

            // Busy-poll while there's recent work, so a -c1 client's next request
            // is reaped immediately (the old unconditional 200µs sleep added that
            // latency to every request: ~3.8k rps / 0.26ms). Only yield the core
            // once we've been idle a while, so a quiet shard doesn't spin at 100%.
            // (A proper IORING_OP_TIMEOUT / waker-poll park is the real fix.)
            if comps.is_empty() && !did_inbound {
                idle_spins = idle_spins.saturating_add(1);
                if idle_spins >= URING_SPIN_LIMIT {
                    std::thread::sleep(Duration::from_micros(200));
                }
            } else {
                idle_spins = 0;
            }
        }
        Ok(())
    }

    /// Submit a read for every idle open conn and a write for every conn with
    /// pending output, reusing one fixed buffer per direction per conn.
    fn uring_arm_conns(
        &mut self,
        ring: &mut IoUring,
        io: &mut KevyMap<u64, UringConn>,
        cids: &mut Vec<u64>,
        bgid: u16,
    ) {
        cids.clear();
        cids.extend(self.conns.keys().copied());
        for &cid in cids.iter() {
            // Start a new write: move the conn's output into the stable write_buf.
            if let (Some(uc), Some(conn)) = (io.get_mut(&cid), self.conns.get_mut(&cid))
                && !uc.write_inflight
                && uc.write_buf.is_empty()
                && !conn.output.is_empty()
            {
                std::mem::swap(&mut uc.write_buf, &mut conn.output);
                uc.write_off = 0;
            }
            // Submit the write (fresh or a partial-write continuation).
            let write_req = io.get(&cid).map(|uc| {
                (!uc.write_inflight && uc.write_off < uc.write_buf.len(), uc.write_off)
            });
            if let Some((true, off)) = write_req {
                let fd = self.conns[&cid].sock.raw();
                let uc = &io[&cid];
                // SAFETY: write_buf is owned, stable, and outlives the SQE.
                let ok = unsafe {
                    ring.prep_write(
                        fd,
                        uc.write_buf.as_ptr().add(off),
                        (uc.write_buf.len() - off) as u32,
                        OP_WRITE | cid,
                    )
                };
                if ok {
                    io.get_mut(&cid).unwrap().write_inflight = true;
                }
            }
            // Arm a multishot recv if one isn't already running (it re-fires per
            // arrival into the shared provided-buffer ring, so this happens once
            // per connection, not once per read — the syscall-batching win).
            let want_recv = io.get(&cid).is_some_and(|uc| !uc.recv_armed && !uc.closing);
            if want_recv {
                let fd = self.conns[&cid].sock.raw();
                if ring.prep_recv_multishot(fd, bgid, OP_RECV | cid) {
                    io.get_mut(&cid).unwrap().recv_armed = true;
                }
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
            if c.res != -ENOBUFS
                && let Some(uc) = io.get_mut(&cid)
            {
                uc.closing = true;
            }
            return;
        }
        // res > 0: a buffer was filled; copy it out and return it to the ring.
        let Some(bid) = c.buffer_id() else {
            return; // no buffer (shouldn't happen for a successful recv)
        };
        let n = c.res as usize;
        if let Some(conn) = self.conns.get_mut(&cid) {
            conn.input.extend_from_slice(pbuf.bytes(bid, n));
        }
        pbuf.recycle(bid);
        // Zero-alloc parse hot path: mirrors the epoll path.
        // parse_command_into reuses self.scratch_argv; mem::replace dance
        // lets handle_command take &mut self while the parsed argv sits on
        // the stack.
        let mut had_protocol_error = false;
        loop {
            let consumed = {
                let Some(conn) = self.conns.get_mut(&cid) else {
                    return;
                };
                match parse_command_into(&conn.input, &mut self.scratch_argv) {
                    Ok(Some(c)) => Some(c),
                    Ok(None) => None,
                    Err(_) => {
                        had_protocol_error = true;
                        None
                    }
                }
            };
            match consumed {
                Some(c) => {
                    if let Some(conn) = self.conns.get_mut(&cid) {
                        conn.input.drain(..c);
                    } else {
                        return;
                    }
                    let argv = std::mem::take(&mut self.scratch_argv);
                    if let Some(key) = argv.get(1) {
                        self.store.prefetch_for_key(key);
                    }
                    self.handle_command(cid, &argv);
                    self.scratch_argv = argv;
                }
                None => break,
            }
        }
        if had_protocol_error {
            self.protocol_error(cid);
            if let Some(uc) = io.get_mut(&cid) {
                uc.closing = true;
            }
        }
    }

    /// A write completed: advance progress; resubmit the remainder next loop.
    fn uring_on_write(&mut self, cid: u64, res: i32, io: &mut KevyMap<u64, UringConn>) {
        let Some(uc) = io.get_mut(&cid) else {
            return;
        };
        uc.write_inflight = false;
        if res < 0 {
            uc.closing = true;
            return;
        }
        uc.write_off += res as usize;
        if uc.write_off >= uc.write_buf.len() {
            uc.write_buf.clear();
            uc.write_off = 0;
        }
    }

    /// Drain cross-core rings: execute forwarded requests, fold replies into
    /// their connection's output (no direct write — io_uring flushes it).
    fn uring_drain_inbound(&mut self) -> bool {
        let mut did = false;
        for src in 0..self.nshards {
            if src == self.id {
                continue;
            }
            while let Some(msg) = self.inboxes[src].as_mut().expect("peer inbox").pop() {
                did = true;
                match msg {
                    Inbound::Request { origin, conn, seq, op } => {
                        let part = self.exec_op(op);
                        self.send_to(origin, Inbound::Response { conn, seq, part });
                    }
                    Inbound::Response { conn, seq, part } => {
                        self.fold(conn, seq, part);
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
                    // Batched replies: fold each by seq; the arm loop writes any
                    // conn whose output this appended to.
                    Inbound::ResponseBatch(resps) => {
                        for (conn, seq, part) in resps {
                            self.fold(conn, seq, part);
                        }
                    }
                    // Fire-and-forget batched pub/sub delivery; the arm loop
                    // writes any conn whose output this appended to.
                    Inbound::DeliverPublish(batch) => {
                        for m in &batch {
                            self.deliver_publish(&m.0, &m.1);
                        }
                    }
                }
            }
        }
        did
    }

    /// Close connections that are done: EOF/QUIT seen, all output flushed, no
    /// SQE in flight. Dropping the `Conn` closes the fd.
    fn uring_reap_closed(&mut self, io: &mut KevyMap<u64, UringConn>) {
        let done: Vec<u64> = io
            .iter()
            .filter(|(cid, uc)| {
                let conn = self.conns.get(cid);
                let drained = conn.is_none_or(|c| {
                    c.output.is_empty() && c.pending.is_empty() && c.write_pos == 0
                });
                let closing = uc.closing || conn.is_some_and(|c| c.closing);
                // The multishot recv may still be armed; closing the fd (on Conn
                // drop) terminates it and its final completion is ignored (conn
                // gone). We only need writes fully flushed before closing.
                closing && !uc.write_inflight && uc.write_buf.is_empty() && drained
            })
            .map(|(&cid, _)| cid)
            .collect();
        for cid in done {
            if let Some(c) = self.conns.remove(&cid) {
                self.unregister_subs(&c.sub); // Conn drop closes the socket fd
            }
            io.remove(&cid);
        }
    }
}
