//! Per-completion I/O handlers for the io_uring reactor: recv pump (with
//! provided-buffer copy-out + dispatch), write progress, and the
//! mark-closing teardown helper. Split out of [`crate::uring_reactor`] so
//! that file stays under the 500-LOC house rule; every method here is on
//! the same `impl<C: Commands> Shard<C>` and is only ever called from
//! `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::UringConn;
use crate::uring_reactor::ENOBUFS;
use kevy_map::KevyMap;
use kevy_uring::{Completion, ProvidedBufRing};

impl<C: Commands> Shard<C> {
    /// A multishot recv completed: copy the kernel-picked buffer's bytes into the
    /// conn, recycle it, run every complete command, and re-arm if the SQE ended.
    pub(crate) fn uring_on_recv(
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
    pub(crate) fn uring_mark_closing(&mut self, cid: u64, io: &mut KevyMap<u64, UringConn>) {
        if let Some(uc) = io.get_mut(&cid) {
            uc.closing = true;
        }
        self.blocked.drop_for_conn(cid);
        self.cancel_xshard_on_close(cid);
    }

    /// A write completed: advance progress; resubmit the remainder next loop.
    pub(crate) fn uring_on_write(
        &mut self,
        cid: u64,
        res: i32,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        let Some(uc) = io.get_mut(&cid) else {
            return;
        };
        uc.write_inflight = false;
        if res < 0 {
            self.uring_mark_closing(cid, io);
            return;
        }
        // L1 (2026-06-21): the writev path mixes write_buf bytes with
        // arc-bulk borrowed bytes via the iovec list. On a full
        // completion (res == total submitted) we clear everything and
        // drop the Arcs; on a SHORT write we fall back — copy any
        // unsent arc payloads into write_buf so the next iter can
        // resume via the plain `prep_write` path. The short-write case
        // is rare (TCP loopback rarely backpressures); this fallback
        // preserves correctness without iovec-level resume bookkeeping.
        if !uc.write_arcs.is_empty() {
            let written = res as usize;
            let total: usize = uc.write_iovecs.iter().map(|v| v.iov_len).sum();
            if written == total {
                uc.write_buf.clear();
                uc.write_arcs.clear();
                uc.write_iovecs.clear();
                uc.write_off = 0;
            } else {
                // Materialise the full payload into write_buf; drop arcs;
                // advance write_off to where we left off. Next iter
                // submits the remainder via the simple prep_write path.
                let mut linear: Vec<u8> = Vec::with_capacity(total);
                let mut prev = 0usize;
                for (pos, arc) in &uc.write_arcs {
                    let pos = *pos;
                    if pos > prev {
                        linear.extend_from_slice(&uc.write_buf[prev..pos]);
                    }
                    linear.extend_from_slice(arc.as_ref());
                    prev = pos;
                }
                if prev < uc.write_buf.len() {
                    linear.extend_from_slice(&uc.write_buf[prev..]);
                }
                uc.write_buf = linear;
                uc.write_arcs.clear();
                uc.write_iovecs.clear();
                uc.write_off = written;
            }
            return;
        }
        uc.write_off += res as usize;
        if uc.write_off >= uc.write_buf.len() {
            uc.write_buf.clear();
            uc.write_off = 0;
        }
    }
}
