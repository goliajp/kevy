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
use crate::message::Inbound;
use crate::shard::Shard;
use kevy_persist::{load_snapshot, replay_aof};
use kevy_resp::parse_command;
use kevy_sys::{Completion, IoUring, Socket};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// SQ/CQ depth for the per-shard ring.
const URING_ENTRIES: u32 = 256;
/// Fixed per-connection read buffer (bytes); larger requests span several reads.
const READ_BUF: usize = 16 * 1024;

// `user_data` layout: top 2 bits = op, low 62 bits = conn id.
const OP_SHIFT: u32 = 62;
const OP_READ: u64 = 1 << OP_SHIFT;
const OP_WRITE: u64 = 2 << OP_SHIFT;
const OP_ACCEPT: u64 = 3 << OP_SHIFT;
const CONN_MASK: u64 = (1 << OP_SHIFT) - 1;

/// io_uring-specific per-connection state (the byte buffers that must outlive
/// their in-flight SQEs). The command-level state stays in the shard's [`Conn`].
struct UringConn {
    /// Read target; an in-flight read SQE points here, so it must not move.
    read_buf: Vec<u8>,
    read_inflight: bool,
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
            read_buf: vec![0u8; READ_BUF],
            read_inflight: false,
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
        let mut io: HashMap<u64, UringConn> = HashMap::new();
        let mut accept_inflight = false;
        let mut comps: Vec<Completion> = Vec::with_capacity(URING_ENTRIES as usize);
        let mut cids: Vec<u64> = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            // Always keep one accept in flight.
            if !accept_inflight {
                accept_inflight = ring.prep_accept(self.listener.raw(), OP_ACCEPT);
            }
            self.uring_arm_conns(&mut ring, &mut io, &mut cids);

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
                    OP_READ => {
                        if let Some(uc) = io.get_mut(&cid) {
                            uc.read_inflight = false;
                        }
                        self.uring_on_read(cid, c.res, &mut io);
                    }
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
            self.flush_wakes();
            if let Some(aof) = &mut self.aof {
                let _ = aof.maybe_sync();
            }
            self.uring_reap_closed(&mut io);

            // Idle: yield the core briefly so a quiet shard doesn't spin at 100%
            // (a proper IORING_OP_TIMEOUT park is a follow-up).
            if comps.is_empty() && !did_inbound {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        Ok(())
    }

    /// Submit a read for every idle open conn and a write for every conn with
    /// pending output, reusing one fixed buffer per direction per conn.
    fn uring_arm_conns(
        &mut self,
        ring: &mut IoUring,
        io: &mut HashMap<u64, UringConn>,
        cids: &mut Vec<u64>,
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
            // Submit a read if none is in flight and we're not closing.
            let want_read = io.get(&cid).is_some_and(|uc| !uc.read_inflight && !uc.closing);
            if want_read {
                let fd = self.conns[&cid].sock.raw();
                let uc = io.get_mut(&cid).unwrap();
                // SAFETY: read_buf is owned, stable, and outlives the SQE.
                let ok = unsafe {
                    ring.prep_read(fd, uc.read_buf.as_mut_ptr(), uc.read_buf.len() as u32, OP_READ | cid)
                };
                if ok {
                    uc.read_inflight = true;
                }
            }
        }
    }

    /// A read completed: append the bytes and run every complete command.
    fn uring_on_read(&mut self, cid: u64, res: i32, io: &mut HashMap<u64, UringConn>) {
        if res <= 0 {
            if let Some(uc) = io.get_mut(&cid) {
                uc.closing = true; // EOF or error
            }
            return;
        }
        let n = res as usize;
        if let (Some(conn), Some(uc)) = (self.conns.get_mut(&cid), io.get(&cid)) {
            let n = n.min(uc.read_buf.len());
            conn.input.extend_from_slice(&uc.read_buf[..n]);
        } else {
            return;
        }
        loop {
            let parsed = {
                let Some(conn) = self.conns.get_mut(&cid) else {
                    return;
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
                Some(Ok(args)) => self.handle_command(cid, args),
                Some(Err(())) => {
                    self.protocol_error(cid);
                    if let Some(uc) = io.get_mut(&cid) {
                        uc.closing = true;
                    }
                    break;
                }
                None => break,
            }
        }
    }

    /// A write completed: advance progress; resubmit the remainder next loop.
    fn uring_on_write(&mut self, cid: u64, res: i32, io: &mut HashMap<u64, UringConn>) {
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
                }
            }
        }
        did
    }

    /// Close connections that are done: EOF/QUIT seen, all output flushed, no
    /// SQE in flight. Dropping the `Conn` closes the fd.
    fn uring_reap_closed(&mut self, io: &mut HashMap<u64, UringConn>) {
        let done: Vec<u64> = io
            .iter()
            .filter(|(cid, uc)| {
                let conn = self.conns.get(cid);
                let drained = conn.is_none_or(|c| {
                    c.output.is_empty() && c.pending.is_empty() && c.write_pos == 0
                });
                let closing = uc.closing || conn.is_some_and(|c| c.closing);
                closing
                    && !uc.read_inflight
                    && !uc.write_inflight
                    && uc.write_buf.is_empty()
                    && drained
            })
            .map(|(&cid, _)| cid)
            .collect();
        for cid in done {
            self.conns.remove(&cid); // Conn drop closes the socket fd
            io.remove(&cid);
        }
    }
}
