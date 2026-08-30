//! `COPY src dst [REPLACE]` across shards.
//!
//! The source and the destination are different keys, so on a
//! thread-per-core server they routinely live on different cores — the
//! same hazard `exec_listmove` and `exec_rename` were written for. Left
//! to the router's catch-all (`Route::Single(1)`, "hash args[1]") the
//! copy would be written into the source's shard, invisible to every
//! later read of the destination, while the command reported success.
//!
//! Two steps, and deliberately no third. RENAME needs a Restore step
//! because step 1 takes the source away, so an NX-refused put has to
//! give it back. COPY *clones*: a refused put leaves both keys exactly
//! as they were, and the worst a crash between the steps can do is not
//! write the destination. That is the safe direction, and it is why
//! this file is half the length of `exec_rename.rs`.
//!
//! Same-shard pairs take one atomic `Op::Copy` and behave exactly as
//! Redis's COPY does.

use crate::Commands;
use crate::message::{Agg, Inbound, Op, Part, PendingSlot, SmallReply};
use crate::message_agg::CopyStep;
use crate::reduce::drain_front;
use crate::shard::Shard;
use kevy_resp::ArgvView;

/// What a cross-shard copy needs to know, in one place.
///
/// The alternative was eight parameters and the
/// `clippy::too_many_arguments` suppression its RENAME twin carries. A
/// parameter list that long is a struct that has not been written down
/// yet, and the two shard indices in the middle of it are exactly the
/// pair a caller can transpose without the compiler noticing.
struct CopyPlan {
    src: Vec<u8>,
    dst: Vec<u8>,
    src_shard: usize,
    dst_shard: usize,
    replace: bool,
}

impl<C: Commands> Shard<C> {
    /// Entry point for [`crate::Route::Copy`]. `args[1]` is the source,
    /// `args[2]` the destination, and an optional `args[3]` must be the
    /// word REPLACE.
    pub(crate) fn start_copy<A: ArgvView + ?Sized>(&mut self, conn_id: u64, seq: u64, args: &A) {
        let replace = match args.len() {
            3 => false,
            4 if args[3].eq_ignore_ascii_case(b"REPLACE") => true,
            4 => return self.fold_copy_reply(conn_id, seq, b"-ERR syntax error\r\n".to_vec()),
            _ => {
                let err = b"-ERR wrong number of arguments for 'copy' command\r\n".to_vec();
                return self.fold_copy_reply(conn_id, seq, err);
            }
        };
        let (src, dst) = (args[1].to_vec(), args[2].to_vec());
        if src == dst {
            let err = b"-ERR source and destination objects are the same\r\n".to_vec();
            return self.fold_copy_reply(conn_id, seq, err);
        }
        let (src_shard, dst_shard) = (self.shard_of(&src), self.shard_of(&dst));
        if src_shard == dst_shard {
            // The same-shard path takes ONE op, but it folds into the
            // same orchestrator slot as the cross-shard one — at its
            // last step, so `Part::CopyPutDone` lands in `stored` and
            // finalize turns it into the reply. The first draft gave
            // this path an `Agg::First`, which has no arm for that Part:
            // the fold ignored it and the client got the materialize
            // fallback, `-ERR internal error`.
            let agg = Agg::CopyOrchestrator {
                step: CopyStep::Put,
                replace,
                dst: dst.clone(),
                dst_shard,
                read: None,
                stored: None,
            };
            self.push_pending_slot(conn_id, 1, agg, false);
            self.send_or_run(conn_id, seq, src_shard, Op::Copy { src, dst, replace });
            return;
        }
        let plan = CopyPlan { src, dst, src_shard, dst_shard, replace };
        self.start_copy_xshard(conn_id, seq, plan);
    }

    /// Cross-shard arm: one pending slot carrying the orchestrator, and
    /// step 1 out to the source's shard.
    fn start_copy_xshard(&mut self, conn_id: u64, seq: u64, plan: CopyPlan) {
        let agg = Agg::CopyOrchestrator {
            step: CopyStep::Read,
            replace: plan.replace,
            dst: plan.dst,
            dst_shard: plan.dst_shard,
            read: None,
            stored: None,
        };
        self.push_pending_slot(conn_id, 1, agg, false);
        self.send_or_run(conn_id, seq, plan.src_shard, Op::CopyRead(plan.src));
    }

    /// Resume the cross-shard COPY once a sub-reply lands. Called from
    /// `Shard::fold` when an `Agg::CopyOrchestrator` slot empties.
    pub(crate) fn finalize_copy_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::CopyOrchestrator { step, replace, dst, dst_shard, read, stored } = agg else {
            return;
        };
        match step {
            CopyStep::Read => match read.flatten() {
                // No source, no copy — and nothing was disturbed to
                // find that out.
                None => self.fill_copy_slot(conn_id, seq, b":0\r\n".to_vec()),
                Some((value, ttl_ms)) => {
                    let agg = Agg::CopyOrchestrator {
                        step: CopyStep::Put,
                        replace,
                        dst: dst.clone(),
                        dst_shard,
                        read: None,
                        stored: None,
                    };
                    self.rearm_copy_slot(conn_id, seq, agg);
                    let op = Op::CopyPut { dst, value, ttl_ms, replace };
                    self.send_or_run(conn_id, seq, dst_shard, op);
                }
            },
            CopyStep::Put => {
                let bytes = if stored == Some(true) { b":1\r\n" } else { b":0\r\n" };
                self.fill_copy_slot(conn_id, seq, bytes.to_vec());
            }
        }
    }

    /// Same-shard `COPY`: clone the source's value and remaining TTL,
    /// then place it. One op, so it is atomic exactly as Redis's is.
    pub(crate) fn op_copy(&mut self, src: &[u8], dst: Vec<u8>, replace: bool) -> Part {
        let Some((value, ttl_ms)) = self.store.clone_with_ttl(src) else {
            return Part::CopyPutDone { stored: false };
        };
        self.op_copy_put(dst, value, ttl_ms, replace)
    }

    /// Place a cloned value at `dst`, refusing when `dst` exists and
    /// `REPLACE` was not given. The refusal needs no rollback: unlike
    /// RENAME's, this clone was never removed from anywhere.
    pub(crate) fn op_copy_put(
        &mut self,
        dst: Vec<u8>,
        value: kevy_store::Value,
        ttl_ms: Option<u64>,
        replace: bool,
    ) -> Part {
        if !replace && self.store.key_exists(&dst) {
            return Part::CopyPutDone { stored: false };
        }
        self.log_value_placed(&dst, &value, ttl_ms);
        self.store.put_with_ttl(dst.clone(), value, ttl_ms);
        self.note_key_mutated(&dst);
        Part::CopyPutDone { stored: true }
    }

    /// Run `op` here, or ship it to the shard that owns the key.
    fn send_or_run(&mut self, conn_id: u64, seq: u64, shard: usize, op: Op) {
        if shard == self.id {
            let part = self.exec_op(op);
            self.fold(conn_id, seq, part);
        } else {
            let origin = self.id;
            self.send_to(shard, Inbound::Request { origin, conn: conn_id, seq, op });
        }
    }

    /// Put the slot back in flight for step 2.
    fn rearm_copy_slot(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = agg;
            }
        }
    }

    /// Finish the slot with a fixed reply, in seq order.
    fn fill_copy_slot(&mut self, conn_id: u64, seq: u64, bytes: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.done = Some(SmallReply::from_vec(bytes));
            }
            drain_front(c);
        }
    }

    /// The synchronous error paths (arity, syntax, src == dst): push a
    /// slot and fold the reply straight into it, so the answer keeps its
    /// place in the connection's order.
    fn fold_copy_reply(&mut self, conn_id: u64, seq: u64, reply: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto,
            });
        }
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(reply)));
    }
}
