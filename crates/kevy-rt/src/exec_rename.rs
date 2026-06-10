//! `RENAME` / `RENAMENX` orchestration. The runtime decides whether
//! both keys live on the same shard (atomic single-Op route) or split
//! across shards (Take-Put orchestrator landing in v2-3b — until then
//! the cross-shard arm returns `-CROSSSHARD ...`).
//!
//! Why this can't be served by `Route::Single`: a single-key route hits
//! the source shard, where a same-shard atomic is straightforward but a
//! cross-shard hop needs to know nshards + emit a different Op to ship
//! the value to the destination shard. Routing is the runtime's job;
//! the dispatch layer (`kevy::cmd::*`) sees only one shard at a time.

use crate::message::{Agg, Inbound, Op, Part, PendingSlot, RenameStep};
use crate::reduce::drain_front;
use crate::shard::Shard;
use crate::{Commands, RespVersion};
use kevy_resp::ArgvView;

impl<C: Commands> Shard<C> {
    /// `RENAME` / `RENAMENX` — see [`Route::Rename`].
    pub(crate) fn start_rename<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        nx: bool,
    ) {
        // Arity: RENAME source destination → 3 args.
        if args.len() != 3 {
            let cmd_name = if nx { "renamenx" } else { "rename" };
            let err = format!("-ERR wrong number of arguments for '{cmd_name}' command\r\n");
            self.fold_rename_reply(conn_id, seq, err.into_bytes());
            return;
        }
        let src = args[1].to_vec();
        let dst = args[2].to_vec();
        let src_shard = self.shard_of(&src);
        let dst_shard = self.shard_of(&dst);

        if src_shard == dst_shard {
            // Same-shard: one atomic Op::Rename. Route to the owning
            // shard — exec_op runs store.rename + bumps WATCH versions
            // + AOF logs + emits keyspace notifications.
            self.push_pending_slot(conn_id, 1, Agg::First(None), false);
            let op = Op::Rename { src, dst, nx };
            if src_shard == self.id {
                let part = self.exec_op(op);
                self.fold(conn_id, seq, part);
            } else {
                self.send_to(
                    src_shard,
                    Inbound::Request {
                        origin: self.id,
                        conn: conn_id,
                        seq,
                        op,
                    },
                );
            }
            return;
        }

        // Cross-shard: orchestrator. Push a single pending slot with
        // Agg::RenameOrchestrator; step 1 emits Op::RenameTake to
        // src_shard. Fold receives Part::RenameTaken (or NoSuchSrc),
        // step transitions to Put, emits Op::RenamePut. Step 2's
        // Part::RenamePutDone triggers the +OK / :1 / :0 reply.
        let agg = Agg::RenameOrchestrator {
            step: RenameStep::Take,
            nx,
            src: src.clone(),
            dst,
            dst_shard,
            taken: None,
            put_stored: None,
        };
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg,
                done: None,
                proto,
            });
        }
        let take_op = Op::RenameTake(src);
        if src_shard == self.id {
            let part = self.exec_op(take_op);
            self.fold(conn_id, seq, part);
        } else {
            self.send_to(
                src_shard,
                Inbound::Request {
                    origin: self.id,
                    conn: conn_id,
                    seq,
                    op: take_op,
                },
            );
        }
    }

    /// Resume the cross-shard RENAME after a sub-reply lands. Called
    /// from `Shard::fold` once `slot.remaining == 0` for an
    /// `Agg::RenameOrchestrator` slot.
    ///
    /// On step-1 completion: if Take succeeded → ship step 2 to
    /// dst_shard, re-arm the slot. If Take missed → finalize with
    /// `-ERR no such key`.
    ///
    /// On step-2 completion: finalize with `+OK` (RENAME ok) or `:1`
    /// (RENAMENX ok) or `:0` (RENAMENX-blocked: dst already existed
    /// on dst_shard at the moment of Put; we accept the data-loss
    /// race vs adding a third "restore-src" step — Redis cluster has
    /// the same trade-off via MIGRATE).
    pub(crate) fn finalize_rename_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::RenameOrchestrator {
            step,
            nx,
            src,
            dst,
            dst_shard,
            taken,
            put_stored,
        } = agg
        else {
            return;
        };
        match step {
            RenameStep::Take => self.advance_rename_to_put(conn_id, seq, nx, src, dst, dst_shard, taken),
            RenameStep::Put => self.finish_rename_put(conn_id, seq, nx, src, taken, put_stored),
            // Restore (RENAMENX NX-refused) completed → src is back; reply :0.
            RenameStep::Restore => self.fill_rename_slot(conn_id, seq, b":0\r\n".to_vec()),
        }
    }

    /// Step 1 → step 2 transition. If src didn't exist, finalize with
    /// `-ERR no such key`. Otherwise re-arm the slot for Put + ship
    /// Op::RenamePut to dst_shard.
    #[allow(clippy::too_many_arguments)]
    fn advance_rename_to_put(
        &mut self,
        conn_id: u64,
        seq: u64,
        nx: bool,
        src: Vec<u8>,
        dst: Vec<u8>,
        dst_shard: usize,
        taken: Option<(kevy_store::Value, Option<u64>)>,
    ) {
        let Some((value, ttl_ms)) = taken else {
            self.fill_rename_slot(conn_id, seq, b"-ERR no such key\r\n".to_vec()); // NoSuchSrc
            return;
        };
        // Re-arm the slot for Put, keeping `src` so an NX-refused Put can
        // restore the source (the value rides to dst now; fold hands it
        // back into `taken` only on refuse — restore-on-refuse, no loss).
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::RenameOrchestrator {
                    step: RenameStep::Put,
                    nx,
                    src,
                    dst: dst.clone(),
                    dst_shard,
                    taken: None,
                    put_stored: None,
                };
            }
        }
        let put_op = Op::RenamePut { dst, value, ttl_ms, nx };
        if dst_shard == self.id {
            let part = self.exec_op(put_op);
            self.fold(conn_id, seq, part);
        } else {
            self.send_to(
                dst_shard,
                Inbound::Request {
                    origin: self.id,
                    conn: conn_id,
                    seq,
                    op: put_op,
                },
            );
        }
    }

    /// Step 2 finished. Reply +OK / :1 / :0 depending on Put's `stored`
    /// flag + the NX flag. `put_stored` is filled by `Shard::fold` from
    /// `Part::RenamePutDone.stored` before this is called.
    fn finish_rename_put(
        &mut self,
        conn_id: u64,
        seq: u64,
        nx: bool,
        src: Vec<u8>,
        taken: Option<(kevy_store::Value, Option<u64>)>,
        put_stored: Option<bool>,
    ) {
        if put_stored.unwrap_or(false) {
            let reply = if nx { b":1\r\n".to_vec() } else { b"+OK\r\n".to_vec() };
            self.fill_rename_slot(conn_id, seq, reply);
            return;
        }
        // RENAMENX cross-shard, dst already existed at Put time → the
        // rename does NOT happen. Step 1 took `src` off its shard, so put
        // it back (the value rode home in `taken`) before replying `:0` —
        // a no-op RENAMENX must not lose the source key.
        match taken {
            Some((value, ttl_ms)) => self.restore_renamed_src(conn_id, seq, nx, src, value, ttl_ms),
            None => self.fill_rename_slot(conn_id, seq, b":0\r\n".to_vec()),
        }
    }

    /// Step 3 (RENAMENX NX-refused only): put the taken source value back
    /// on src's shard, re-arming the slot for the `Restore` step which
    /// emits the `:0` reply once the put-back lands.
    fn restore_renamed_src(
        &mut self,
        conn_id: u64,
        seq: u64,
        nx: bool,
        src: Vec<u8>,
        value: kevy_store::Value,
        ttl_ms: Option<u64>,
    ) {
        let src_shard = self.shard_of(&src);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::RenameOrchestrator {
                    step: RenameStep::Restore,
                    nx,
                    src: Vec::new(),
                    dst: Vec::new(),
                    dst_shard: 0,
                    taken: None,
                    put_stored: None,
                };
            }
        }
        let restore_op = Op::RenamePut { dst: src, value, ttl_ms, nx: false };
        if src_shard == self.id {
            let part = self.exec_op(restore_op);
            self.fold(conn_id, seq, part);
        } else {
            self.send_to(
                src_shard,
                Inbound::Request { origin: self.id, conn: conn_id, seq, op: restore_op },
            );
        }
    }

    /// Drop a literal RESP frame into the orchestrator's slot + drain.
    fn fill_rename_slot(&mut self, conn_id: u64, seq: u64, bytes: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.done = Some(crate::message::SmallReply::from_vec(bytes));
            }
            drain_front(c);
        }
    }

    /// Push a single pending slot + immediately fold a pre-built reply
    /// into it. Used by the synchronous error paths (arity check,
    /// cross-shard rejection) so the reply preserves seq order without
    /// going through the broader `immediate_reply` (which assigns a
    /// fresh seq — we already have one).
    fn fold_rename_reply(&mut self, conn_id: u64, seq: u64, reply: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto,
            });
            // Silence unused — proto only matters for the few aggs that
            // care about RESP3 shape, not for a fixed-bytes reply.
            let _ = RespVersion::V2;
        }
        self.fold(conn_id, seq, Part::Reply(crate::message::SmallReply::from_vec(reply)));
    }
}
