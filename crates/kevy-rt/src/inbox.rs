//! Inbound event handling: socket-readable, cross-core ring drain, and
//! connection teardown. The event loop (`run`), transport setup
//! (accept_ready, flush_conn, flush_dirty, maybe_auto_rewrite_aof), and
//! cross-shard send/backlog plumbing live in [`crate::shard`]; the
//! *semantics* (routing, execution, reduction) live in [`crate::exec`].
//! Split out so each file stays under the 500-LOC house rule without
//! breaking the established two-impl-block layering.

use std::io;

use kevy_resp::parse_command_borrowed;

use crate::Commands;
use crate::message::{Inbound, Op};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Socket readable: read until WouldBlock, then parse out every full
    /// RESP command and dispatch it.
    ///
    /// The local fast path dispatches straight from an `ArgvBorrowed` view
    /// into the connection's read buffer — no per-cmd memcpy. We swap
    /// `conn.input` onto the stack (`mem::take`) for the parse-and-dispatch
    /// loop so the borrowed argv doesn't conflict with `&mut self`; after
    /// each command we `drain(..consumed)` on the local buf, and finally
    /// swap the buf back into the connection (if it still exists). Cross-
    /// shard / MULTI queue / AOF call `args.to_argv()` at the handoff
    /// juncture; only those paths still materialise an owned `Argv`.
    pub(crate) fn conn_readable(&mut self, conn_id: u64) -> io::Result<()> {
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

        // Swap conn.input onto the stack so parse_command_borrowed can lend
        // it to ArgvBorrowed without colliding with &mut self in dispatch.
        let mut input_buf = match self.conns.get_mut(&conn_id) {
            Some(c) => std::mem::take(&mut c.input),
            None => return Ok(()),
        };

        let mut had_protocol_error = false;
        // Group-commit window for `appendfsync always`: this batch's writes
        // buffer their AOF appends instead of fsyncing per command. The fsync
        // happens once at the reactor loop's `flush_dirty` (covering every
        // conn read + cross-shard reply this iteration), and this conn's reply
        // flush is deferred there too — so durability still precedes reply,
        // with one fsync per loop instead of per command.
        self.aof_begin_group();
        loop {
            let parse = parse_command_borrowed(&input_buf);
            let (argv, consumed) = match parse {
                Ok(Some(t)) => t,
                Ok(None) => break,
                Err(_) => {
                    had_protocol_error = true;
                    break;
                }
            };
            if let Some(key) = argv.get(1) {
                self.store.prefetch_for_key(key);
            }
            self.handle_command(conn_id, &argv);
            drop(argv);
            input_buf.drain(..consumed);
            if !self.conns.contains_key(&conn_id) {
                // Connection closed mid-batch; drop the rest of the buf. Any
                // AOF writes it buffered are fsynced at the loop flush point.
                return Ok(());
            }
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.input = input_buf;
        }
        if had_protocol_error {
            self.protocol_error(conn_id);
        }
        // `always`: defer the reply flush to `flush_dirty`, which fsyncs the
        // whole iteration's buffered writes once before any reply leaves.
        // Every other mode flushes eagerly here (unchanged hot path).
        if self.aof_deferring() {
            self.dirty.push(conn_id);
            Ok(())
        } else {
            self.flush_conn(conn_id)
        }
    }

    /// Whether the AOF is buffering an open group-commit window (`always`
    /// mode mid-batch) — the reactor defers reply flushes to the loop fsync.
    #[inline]
    pub(crate) fn aof_deferring(&self) -> bool {
        self.aof.as_ref().is_some_and(|a| a.is_deferring())
    }

    /// Open the AOF group-commit window (no-op unless AOF is on + policy is
    /// `always`). Bracket a batch of writes with this and [`Self::aof_end_group`]
    /// so an `always` policy fsyncs once per batch instead of per command,
    /// still before the batch's replies are sent.
    #[inline]
    pub(crate) fn aof_begin_group(&mut self) {
        if let Some(aof) = &mut self.aof {
            aof.begin_group();
        }
    }

    /// Close the group-commit window: one fsync for the batch (if any writes
    /// buffered), before replies leave. Errors propagate like other flush
    /// failures.
    #[inline]
    pub(crate) fn aof_end_group(&mut self) -> io::Result<()> {
        if let Some(aof) = &mut self.aof {
            aof.end_group()?;
        }
        Ok(())
    }

    /// Drain inbound cross-core messages from every peer ring; returns
    /// whether any were processed.
    pub(crate) fn drain_inbound(&mut self) -> io::Result<bool> {
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
                    // Batched single-key dispatches to this (owning) shard:
                    // exec each locally, reply as one `ResponseBatch` to the
                    // origin.
                    Inbound::RequestBatch { origin, reqs } => {
                        let mut resps = Vec::with_capacity(reqs.len());
                        self.aof_begin_group();
                        for (conn, seq, argv, proto) in reqs {
                            let part = self.exec_op(Op::Dispatch(argv, proto));
                            resps.push((conn, seq, part));
                        }
                        // fsync the batch's forwarded writes before replying.
                        self.aof_end_group()?;
                        self.send_to(origin, Inbound::ResponseBatch(resps));
                    }
                    // Batched replies: fold each by seq, then flush each
                    // touched conn once (dedup — pipelined replies share a
                    // conn).
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
                    // ── Cross-shard BLOCK arbiter (see `block_xshard`) ──
                    Inbound::BlockArm {
                        origin,
                        conn,
                        key,
                        kind,
                        serve_argv,
                        proto,
                    } => self.target_arm(origin, conn, key, kind, serve_argv, proto),
                    Inbound::BlockReady { conn, key } => self.origin_on_ready(conn, &key),
                    Inbound::BlockServeReq { origin, conn, key } => {
                        let reply = self.target_serve(origin, conn, &key);
                        self.send_to(origin, Inbound::BlockServeResp { conn, key, reply });
                    }
                    Inbound::BlockServeResp { conn, key, reply } => {
                        self.origin_on_serve_resp(conn, key, reply);
                        self.flush_conn(conn)?;
                    }
                    Inbound::BlockCancel { origin, conn } => self.target_cancel(origin, conn),
                }
            }
        }
        Ok(did)
    }

    /// Tear down a closing connection: deregister from the poller, drop
    /// its channel + pattern subscriptions from the shared registries
    /// and the per-shard tables, and release its `Socket` (closing the
    /// fd).
    pub(crate) fn close_conn(&mut self, conn_id: u64) {
        if let Some(conn) = self.conns.remove(&conn_id) {
            let fd = conn.sock.raw();
            let _ = self.poller.delete(fd);
            self.fd_to_conn.remove(&fd);
            // Drop any BLPOP / BRPOP / XREAD BLOCK waiter the closing conn
            // was parked in, across all its watched keys. Cheap fast-out
            // when nothing is blocked (the common case).
            self.blocked.drop_for_conn(conn_id);
            // Cancel any cross-shard block this conn was the origin of, so
            // target shards drop their registrations.
            self.cancel_xshard_on_close(conn_id);
            self.unregister_subs(&conn.sub);
            // Drop the conn's psub local table entries first (`unregister_psubs`
            // reads `psub_local` to decide if our shard bit should be cleared).
            for pat in &conn.psub {
                if let Some(ids) = self.psub_local.get_mut(pat) {
                    ids.retain(|&id| id != conn_id);
                    if ids.is_empty() {
                        self.psub_local.remove(pat);
                    }
                }
            }
            self.unregister_psubs(&conn.psub);
            // conn (and its Socket) dropped here → fd closed.
        }
    }
}
