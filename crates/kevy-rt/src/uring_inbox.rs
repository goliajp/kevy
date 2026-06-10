//! The cross-core drain + connection-reap half of the io_uring reactor.
//! Split out of [`crate::uring_reactor`] to keep that file under the
//! 500-LOC house rule — every method here is on the same
//! `impl<C: Commands> Shard<C>` and only ever called from `run_uring`.

use crate::Commands;
use crate::message::Inbound;
use crate::shard::Shard;
use crate::uring_reactor::UringConn;
use kevy_map::KevyMap;

impl<C: Commands> Shard<C> {
    /// Drain cross-core rings: execute forwarded requests, fold replies into
    /// their connection's output (no direct write — io_uring flushes it).
    pub(crate) fn uring_drain_inbound(&mut self) -> bool {
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
                        self.aof_begin_group();
                        for (conn, seq, argv, proto, meta) in reqs {
                            let part = self.run_dispatch(&argv, proto, meta);
                            // The spent argv husk rides home with the reply;
                            // the origin pools it (see `RespBatch`).
                            resps.push((conn, seq, part, argv));
                        }
                        // fsync the batch's forwarded writes before replying.
                        self.uring_aof_end_group();
                        self.send_to(origin, Inbound::ResponseBatch(resps));
                    }
                    // Batched replies: fold each by seq; the arm loop writes any
                    // conn whose output this appended to.
                    Inbound::ResponseBatch(resps) => {
                        for (conn, seq, part, husk) in resps {
                            self.argv_pool.put(husk);
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
                    // Cross-shard BLOCK arbiter — same handlers as the epoll
                    // path (`crate::inbox`). Conn output appended here is
                    // flushed by the io_uring write loop, so no `flush_conn`.
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
                    }
                    Inbound::BlockCancel { origin, conn } => self.target_cancel(origin, conn),
                }
            }
        }
        did
    }

    /// Close connections that are done: EOF/QUIT seen, all output flushed, no
    /// SQE in flight. Dropping the `Conn` closes the fd.
    pub(crate) fn uring_reap_closed(&mut self, io: &mut KevyMap<u64, UringConn>) {
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
            // Use the shared teardown (not a local conns.remove): it also
            // cancels block waiters (local + cross-shard arbiter) and drops
            // pub/sub + pattern subscriptions. Skipping it leaked a parked
            // BLPOP/XREAD waiter and psub registrations on every io_uring
            // disconnect — a waiter left behind could consume a later push
            // meant for a live client. The epoll-only `poller.delete` /
            // `fd_to_conn` steps inside are harmless no-ops here (io_uring
            // never registered the fd with the readiness poller).
            self.close_conn(cid);
            io.remove(&cid);
        }
    }
}
