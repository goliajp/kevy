//! The single-key lane between shards, both ends of one round trip: the
//! owner runs a [`ReqBatch`] and writes every reply into one buffer, the
//! origin folds them back in order. Same `impl<C: Commands> Shard<C>` as
//! [`crate::inbox`], which calls these from its message dispatch.

use std::io;

use kevy_resp::{ArgvView, RespVersion};

use crate::Commands;
use crate::batch_ends::PREFETCH_AHEAD;
use crate::message::{DispatchMeta, Inbound, Part, PendingSlot, ReqBatch, RespBatch, SmallReply};
use crate::reduce::drain_front;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Run one resolved single-target command against the local store,
    /// appending its reply to `out`, then the meta-driven write
    /// bookkeeping — [`Shard::run_dispatch`] without the copy out.
    #[inline]
    fn run_dispatch_into<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        proto: RespVersion,
        meta: DispatchMeta,
        out: &mut Vec<u8>,
    ) {
        let t0 = self.slowlog_t0();
        crate::exec_dispatch::dispatch_proto(
            &self.commands,
            &mut self.store,
            args,
            proto,
            meta,
            out,
        );
        self.slowlog_maybe(t0, args);
        if meta.is_write {
            self.post_write_housekeeping(args, meta);
        }
    }

    /// The owner's half: run each forwarded command and answer them all
    /// as one [`Inbound::ResponseBatch`] in `spare`. Runs from the inbox
    /// only, so it inlines into it.
    #[inline(always)]
    pub(crate) fn run_request_batch<const DIRECT_FLUSH: bool>(
        &mut self,
        origin: usize,
        mut reqs: ReqBatch,
        spare: RespBatch,
    ) -> io::Result<()> {
        let mut resps = spare;
        resps.items.reserve(reqs.len());
        // Fsync-only: the batch aggregates INDEPENDENT commands from
        // different conns — marking them atomic would promise more than
        // each origin did.
        let w0 = self.always_hold_w0();
        self.aof_begin_fsync_window();
        // a request's bucket is fetched while the ones PREFETCH_AHEAD
        // before it run, so its probe finds the line in cache rather than
        // waiting on memory
        for r in reqs.iter().take(PREFETCH_AHEAD) {
            self.prefetch_request(&r.2, r.4);
        }
        for i in 0..reqs.len() {
            if let Some(r) = reqs.get(i + PREFETCH_AHEAD) {
                self.prefetch_request(&r.2, r.4);
            }
            let (conn, seq, ref mut argv, proto, meta) = reqs[i];
            let argv = std::mem::take(argv);
            let start = resps.bytes.len();
            self.run_dispatch_into(&argv, proto, meta, &mut resps.bytes);
            if let Some(t) = self.send_ext(true) {
                // held for acknowledgements: the reply leaves the buffer
                let part = Part::Reply(SmallReply::from_slice(&resps.bytes[start..]));
                resps.bytes.truncate(start);
                self.hold_ext(t, crate::exec_ext::Deliver::Remote { origin, w0, conn, seq, part });
                continue;
            }
            let len = (resps.bytes.len() - start) as u32;
            // The spent argv husk rides home with the reply; the origin
            // pools it (see `RespBatch`).
            resps.items.push((conn, seq, Part::Spanned(len), argv));
        }
        reqs.clear();
        // fsync the batch's forwarded writes before replying.
        if DIRECT_FLUSH {
            self.aof_end_group()?;
        } else {
            self.aof_end_group_logged();
        }
        let batch = Inbound::ResponseBatch { resps, spare: reqs };
        self.send_or_hold_response(w0, origin, batch);
        Ok(())
    }

    /// The origin's half: fold each reply by seq, then flush each touched
    /// conn once (pipelined replies share a conn).
    #[inline(always)]
    pub(crate) fn fold_response_batch<const DIRECT_FLUSH: bool>(
        &mut self,
        src: usize,
        mut resps: RespBatch,
        spare: ReqBatch,
    ) -> io::Result<()> {
        self.xshard_inflight = self.xshard_inflight.saturating_sub(resps.items.len() as u64);
        let mut to_flush = std::mem::take(&mut self.request_batch[src].to_flush);
        let mut at = 0usize;
        for (conn, seq, part, husk) in resps.items.drain(..) {
            self.argv_pool.put(husk);
            match part {
                Part::Spanned(len) => {
                    let end = at + len as usize;
                    self.fold_bytes(conn, seq, &resps.bytes[at..end]);
                    at = end;
                }
                part => self.fold(conn, seq, part),
            }
            if DIRECT_FLUSH {
                if !to_flush.contains(&conn) {
                    to_flush.push(conn);
                }
            } else {
                // See the `Inbound::Response` branch of the inbox for the
                // rationale.
                self.mark_pending_write_dirty(conn);
            }
        }
        resps.bytes.clear();
        self.request_batch[src].recycle(resps, spare);
        for &conn in &to_flush {
            self.flush_conn(conn)?;
        }
        to_flush.clear();
        self.request_batch[src].to_flush = to_flush;
        Ok(())
    }

    /// [`Shard::fold`] for a plain reply the caller still borrows: the
    /// next reply a conn waits for, from a single target, goes straight to
    /// its output; any other is copied out and folded.
    #[inline]
    fn fold_bytes(&mut self, conn_id: u64, seq: u64, reply: &[u8]) {
        if let Some(conn) = crate::conn::conn_at(&mut self.conns, &mut self.conn_slot_hint, conn_id)
            && seq == conn.next_emit
            && matches!(conn.pending.front(), Some(PendingSlot { remaining: 1, agg: None, .. }))
        {
            conn.output.extend_from_slice(reply);
            conn.pending.pop_front();
            conn.next_emit += 1;
            drain_front(conn);
            return;
        }
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(reply)));
    }
}
