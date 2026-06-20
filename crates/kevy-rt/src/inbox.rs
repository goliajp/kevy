//! Inbound event handling: socket-readable, cross-core ring drain, and
//! connection teardown. The event loop (`run`), transport setup
//! (accept_ready, flush_conn, flush_dirty, maybe_auto_rewrite_aof), and
//! cross-shard send/backlog plumbing live in [`crate::shard`]; the
//! *semantics* (routing, execution, reduction) live in [`crate::exec`].
//! Split out so each file stays under the 500-LOC house rule without
//! breaking the established two-impl-block layering.

use std::io;
use std::sync::atomic::Ordering;

use kevy_resp::parse_command_borrowed;

use crate::Commands;
use crate::message::Inbound;
use crate::shard::Shard;

/// What [`Shard::dispatch_batch`] saw: how far the parse cursor got,
/// whether it stopped on malformed input, and whether the conn was
/// closed by one of its own commands (QUIT) mid-batch.
pub(crate) struct BatchOutcome {
    pub(crate) consumed: usize,
    pub(crate) protocol_error: bool,
    pub(crate) conn_gone: bool,
}

impl<C: Commands> Shard<C> {
    /// Parse and dispatch every complete RESP command at the front of
    /// `buf` (the borrowed-argv hot path shared by both reactors). The
    /// caller owns buffer bookkeeping (tail retention) and the AOF
    /// group-commit window around the batch.
    pub(crate) fn dispatch_batch(&mut self, conn_id: u64, buf: &[u8]) -> BatchOutcome {
        let mut off = 0usize;
        loop {
            match parse_command_borrowed(&buf[off..]) {
                Ok(Some((argv, consumed))) => {
                    if let Some(key) = argv.get(1) {
                        self.store.prefetch_for_key(key);
                    }
                    self.handle_command(conn_id, &argv);
                    drop(argv);
                    off += consumed;
                    if !self.conns.contains_key(&conn_id) {
                        return BatchOutcome { consumed: off, protocol_error: false, conn_gone: true };
                    }
                }
                Ok(None) => {
                    return BatchOutcome { consumed: off, protocol_error: false, conn_gone: false };
                }
                Err(_) => {
                    return BatchOutcome { consumed: off, protocol_error: true, conn_gone: false };
                }
            }
        }
    }

    /// Socket readable: read until WouldBlock, then parse out every full
    /// RESP command and dispatch it.
    ///
    /// The local fast path dispatches straight from an `ArgvBorrowed` view
    /// into the connection's read buffer — no per-cmd memcpy. We swap
    /// `conn.input` onto the stack (`mem::take`) for the parse-and-dispatch
    /// loop so the borrowed argv doesn't conflict with `&mut self`; a
    /// cursor advances past each command and ONE final `drain` moves the
    /// (usually empty) unparsed tail to the front before the buf is
    /// swapped back into the connection (if it still exists). Cross-
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
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {} // retry the read
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

        // Group-commit window for `appendfsync always`: the writes dispatched
        // from this pipelined read batch buffer their AOF appends and fsync
        // once in `aof_end_group`, BEFORE `flush_conn` sends their replies.
        self.aof_begin_group();
        let outcome = self.dispatch_batch(conn_id, &input_buf);
        // fsync the batch's buffered writes before any reply leaves the shard.
        self.aof_end_group()?;
        if outcome.conn_gone {
            // Connection was closed mid-batch; drop the rest of the buf.
            return Ok(());
        }
        // ONE tail drain (usually empty) — `dispatch_batch`'s cursor already
        // walked the cmds, so nothing memmoves per command.
        input_buf.drain(..outcome.consumed);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.input = input_buf;
        }
        if outcome.protocol_error {
            self.protocol_error(conn_id);
        }
        self.flush_conn(conn_id)
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

    /// Close the group-commit window, best-effort: a sync error is logged
    /// like other AOF failures. The io_uring drain takes this arm — its
    /// handlers return `()`, so there is nowhere to propagate to.
    #[inline]
    pub(crate) fn aof_end_group_logged(&mut self) {
        if let Err(e) = self.aof_end_group() {
            eprintln!("kevy: shard {} aof group sync failed: {e}", self.id);
        }
    }

    /// Drain inbound cross-core messages from every peer ring; returns
    /// whether any were processed (the epoll reactor's entry point).
    ///
    /// **E15 (2026-06-20)** fast-path split: the post-v1.24-chain perf
    /// diagnostic showed `uring_drain_inbound` at 3.59 % self at -c1
    /// despite the E8 Acquire-load short-circuit — almost all of that
    /// was the cost of *calling* a non-trivial monomorphised function
    /// per busy-poll iter. Split the fast-path Acquire check into a
    /// tiny `#[inline]` wrapper that LLVM can fold into the reactor
    /// loop body and outline the cold drain logic as
    /// `#[inline(never)]` so its bulk stays off the hot iTLB pages.
    #[inline]
    pub(crate) fn drain_inbound(&mut self) -> io::Result<bool> {
        let me = self.id;
        if self.inbound_dirty[me].load(Ordering::Acquire) == 0 {
            return Ok(false);
        }
        self.drain_inbound_core_slow::<true>()
    }

    /// Outlined-cold drain body — only called once the fast-path Acquire
    /// load saw a non-zero dirty mask. Atomically swaps the mask to 0,
    /// walks each set bit's peer ring, and dispatches whatever it finds.
    ///
    /// `DIRECT_FLUSH` selects the epoll behavior (write each touched
    /// conn's output here, propagating I/O errors) over the io_uring one
    /// (`false`: appended output is picked up by the arm/write loop, and
    /// the only fallible step — the AOF group sync — downgrades to a
    /// logged error, so no `Err` is ever built).
    #[inline(never)]
    pub(crate) fn drain_inbound_core_slow<const DIRECT_FLUSH: bool>(
        &mut self,
    ) -> io::Result<bool> {
        // E8 / E15: callers already paid the Acquire load. We do the
        // `lock xchg` swap unconditionally here. AcqRel-on-swap
        // synchronises with the Release `fetch_or` in `send_to`; bits
        // set BETWEEN the caller's load and our swap are still
        // atomically captured.
        let me = self.id;
        let dirty = self.inbound_dirty[me].swap(0, Ordering::AcqRel);
        if dirty == 0 {
            // A peer raced — caller observed bit, but a concurrent
            // drainer already cleared it. Defensive (single-drainer per
            // shard today, so this branch is dead).
            return Ok(false);
        }
        let mut did = false;
        let mut mask = dirty;
        while mask != 0 {
            let src = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            if src == me {
                continue; // no self-ring (defensive — we never OR our own bit)
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
                        if DIRECT_FLUSH {
                            self.flush_conn(conn)?;
                        }
                    }
                    // Batched single-key dispatches to this (owning) shard:
                    // exec each locally, reply as one `ResponseBatch` to the
                    // origin.
                    Inbound::RequestBatch { origin, reqs } => {
                        let mut resps = Vec::with_capacity(reqs.len());
                        self.aof_begin_group();
                        for (conn, seq, argv, proto, meta) in reqs {
                            let part = self.run_dispatch(&argv, proto, meta);
                            // The spent argv husk rides home with the reply;
                            // the origin pools it (see `RespBatch`).
                            resps.push((conn, seq, part, argv));
                        }
                        // fsync the batch's forwarded writes before replying.
                        if DIRECT_FLUSH {
                            self.aof_end_group()?;
                        } else {
                            self.aof_end_group_logged();
                        }
                        self.send_to(origin, Inbound::ResponseBatch(resps));
                    }
                    // Batched replies: fold each by seq, then flush each
                    // touched conn once (dedup — pipelined replies share a
                    // conn).
                    Inbound::ResponseBatch(resps) => {
                        let mut to_flush: Vec<u64> = Vec::new();
                        for (conn, seq, part, husk) in resps {
                            self.argv_pool.put(husk);
                            self.fold(conn, seq, part);
                            if DIRECT_FLUSH && !to_flush.contains(&conn) {
                                to_flush.push(conn);
                            }
                        }
                        for conn in to_flush {
                            self.flush_conn(conn)?;
                        }
                    }
                    // Fire-and-forget batched pub/sub delivery; appended
                    // subscriber output is flushed via `flush_dirty` (epoll)
                    // or the arm/write loop (io_uring).
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
                        if DIRECT_FLUSH {
                            self.flush_conn(conn)?;
                        }
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
