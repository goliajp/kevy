//! `RPOPLPUSH` / `LMOVE` / `BRPOPLPUSH` across shards.
//!
//! The source and the destination are different keys, so on a
//! thread-per-core server they routinely live on different cores. Until
//! this module existed, these verbs fell through the router's catch-all
//! (`Route::Single(1)`, "hash args[1]") and executed entirely on the
//! SOURCE's shard — including the push. The element was written into a
//! keyspace nobody would ever look in, while the command returned the moved
//! value, so the caller believed it had worked. On an 8-shard server 11 of
//! 12 moves lost the element. The store layer knew: `list_ops.rs` said the
//! helpers "operate on whatever the local `Store` holds for `dst`" and left
//! the orchestrator as "a later runtime concern". This is that concern.
//!
//! Same-shard pairs take one atomic Op and behave exactly as Redis does.
//! Cross-shard pairs run Take → Push, with a Restore step that fires only if
//! the destination refuses the element (WRONGTYPE) — so the element is never
//! dropped on the floor.
//!
//! **The cross-shard path is not atomic.** Between Take and Push the element
//! is in neither list; a crash there loses it. A job queue that relies on
//! Redis's atomic RPOPLPUSH must co-locate its two keys with a `{hashtag}`,
//! which routes both to one shard and restores the atomic path. This is the
//! same trade-off `exec_rename` makes, and it is stated in `docs/migration.md`.

use crate::message::{Agg, Inbound, Op, PendingSlot, SmallReply};
use crate::message_agg::ListMoveStep;
use crate::reduce::drain_front;
use crate::shard::Shard;
use crate::Commands;
use kevy_resp::ArgvView;

impl<C: Commands> Shard<C> {
    /// Entry point for [`crate::Route::ListMove`]. `args[1]` is the source
    /// and `args[2]` the destination for every verb in the family.
    pub(crate) fn start_list_move<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        from_left: bool,
        to_left: bool,
    ) {
        self.start_list_move_inner(conn_id, seq, &args[1], &args[2], from_left, to_left, false);
    }

    /// Shared body. `blocking` = we are serving a parked `BRPOPLPUSH`, so the
    /// terminal reply is routed back through the block arbiter instead of out
    /// through the pending slot.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_list_move_inner(
        &mut self,
        conn_id: u64,
        seq: u64,
        src_key: &[u8],
        dst_key: &[u8],
        from_left: bool,
        to_left: bool,
        blocking: bool,
    ) {
        let src = src_key.to_vec();
        let dst = dst_key.to_vec();
        let src_shard = self.shard_of(&src);
        let dst_shard = self.shard_of(&dst);

        if src_shard == dst_shard && !blocking {
            // One shard owns both keys: the whole move is a single atomic Op,
            // exactly Redis's semantics. `{hashtag}`-co-located keys always
            // land here.
            self.push_pending_slot(conn_id, 1, Agg::First(None), false);
            let op = Op::ListMove { src, dst, from_left, to_left };
            self.dispatch_op(conn_id, seq, src_shard, op);
            return;
        }

        self.start_list_move_xshard(conn_id, seq, src, dst, src_shard, dst_shard, from_left, to_left, blocking);
    }

    /// Cross-shard arm: arm the orchestrator slot and ship step 1.
    #[allow(clippy::too_many_arguments)]
    fn start_list_move_xshard(
        &mut self,
        conn_id: u64,
        seq: u64,
        src: Vec<u8>,
        dst: Vec<u8>,
        src_shard: usize,
        dst_shard: usize,
        from_left: bool,
        to_left: bool,
        blocking: bool,
    ) {
        let agg = Agg::ListMoveOrchestrator {
            step: ListMoveStep::Take,
            blocking,
            src: src.clone(),
            dst,
            src_shard,
            dst_shard,
            from_left,
            to_left,
            taken: None,
            pushed: None,
        };
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot { remaining: 1, agg, done: None, proto });
        }
        self.dispatch_op(conn_id, seq, src_shard, Op::ListMoveTake { key: src, from_left });
    }

    /// Run `op` on `shard` — inline when we already are that shard, otherwise
    /// as a cross-core request. Both arms land in `fold`.
    fn dispatch_op(&mut self, conn_id: u64, seq: u64, shard: usize, op: Op) {
        if shard == self.id {
            let part = self.exec_op(op);
            self.fold(conn_id, seq, part);
        } else {
            self.send_to(shard, Inbound::Request { origin: self.id, conn: conn_id, seq, op });
        }
    }

    /// Advance the orchestrator once a step's reply has landed. Called from
    /// `fold` when the slot's `remaining` hits zero.
    pub(crate) fn finalize_list_move_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::ListMoveOrchestrator {
            step, blocking, src, dst, src_shard, dst_shard, from_left, to_left, taken, pushed,
        } = agg
        else {
            return;
        };

        match step {
            ListMoveStep::Take => match taken {
                // Source empty or absent. For a plain move that is a nil
                // reply. For a parked BRPOPLPUSH it means another client
                // drained the source between the readiness signal and our
                // Take — hand an EMPTY reply back to the arbiter, which
                // re-arms the watchers and keeps the conn parked.
                None | Some(Ok(None)) => {
                    let miss = if blocking { Vec::new() } else { b"$-1\r\n".to_vec() };
                    self.finish_list_move(conn_id, blocking, &src, miss);
                }
                Some(Err(())) => self.finish_list_move(conn_id, blocking, &src, wrongtype()),
                Some(Ok(Some(value))) => {
                    self.rearm(
                        conn_id,
                        Agg::ListMoveOrchestrator {
                            step: ListMoveStep::Push,
                            blocking,
                            src,
                            dst: dst.clone(),
                            src_shard,
                            dst_shard,
                            from_left,
                            to_left,
                            taken: Some(Ok(Some(value.clone()))),
                            pushed: None,
                        },
                    );
                    self.dispatch_op(
                        conn_id,
                        seq,
                        dst_shard,
                        Op::ListMovePush { key: dst, value, to_left },
                    );
                }
            },
            ListMoveStep::Push => {
                let element = match taken {
                    Some(Ok(Some(v))) => v,
                    // Unreachable by construction: we only enter Push with an
                    // element in hand. Reply nil rather than panic in a reactor.
                    _ => return self.finish_list_move(conn_id, blocking, &src, b"$-1\r\n".to_vec()),
                };
                if pushed == Some(true) {
                    let mut out = Vec::with_capacity(element.len() + 16);
                    kevy_resp::encode_bulk(&mut out, &element);
                    return self.finish_list_move(conn_id, blocking, &src, out);
                }
                // The destination exists and is not a list. Put the element
                // back where it came from before telling the client — a
                // WRONGTYPE must not cost them their data.
                self.rearm(
                    conn_id,
                    Agg::ListMoveOrchestrator {
                        step: ListMoveStep::Restore,
                        blocking,
                        src: src.clone(),
                        dst,
                        src_shard,
                        dst_shard,
                        from_left,
                        to_left,
                        taken: Some(Ok(Some(element.clone()))),
                        pushed: Some(false),
                    },
                );
                self.dispatch_op(
                    conn_id,
                    seq,
                    src_shard,
                    Op::ListMoveRestore { key: src, value: element, from_left },
                );
            }
            // The element is back on the source; the client gets the error the
            // destination raised.
            ListMoveStep::Restore => self.finish_list_move(conn_id, blocking, &src, wrongtype()),
        }
    }

    /// Re-arm the orchestrator slot for the next step.
    fn rearm(&mut self, conn_id: u64, agg: Agg) {
        if let Some(c) = self.conns.get_mut(&conn_id)
            && let Some(slot) = c.pending.front_mut()
        {
            slot.remaining = 1;
            slot.agg = agg;
        }
    }

    /// Finish the orchestration.
    ///
    /// A plain move flushes through its pending slot like any other command.
    /// A parked `BRPOPLPUSH` must instead hand the bytes back to the block
    /// arbiter: only it knows how to unpark the conn and cancel its other
    /// watchers on a hit, and how to re-arm on an empty (raced) reply. The
    /// orchestrator's slot is dropped in that case — it was only ever the
    /// state machine's holder, never the reply's route.
    fn finish_list_move(&mut self, conn_id: u64, blocking: bool, src: &[u8], bytes: Vec<u8>) {
        if blocking {
            if let Some(c) = self.conns.get_mut(&conn_id) {
                c.pending.pop_front();
            }
            self.origin_on_serve_resp(conn_id, src.to_vec(), bytes);
            return;
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            if let Some(slot) = c.pending.front_mut() {
                slot.remaining = 0;
                slot.done = Some(SmallReply::from_vec(bytes));
            }
            drain_front(c);
        }
    }
}

fn wrongtype() -> Vec<u8> {
    b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
}

impl<C: Commands> Shard<C> {
    /// AOF + notifications for the same-shard atomic move. Logged as its two
    /// effects rather than as the verb, so the record is identical to the
    /// cross-shard path's — a replay does not have to know which shard layout
    /// produced it.
    pub(crate) fn after_list_move(&mut self, src: &[u8], dst: &[u8], from_left: bool, to_left: bool) {
        self.store.bump_if_watched(src);
        self.store.bump_if_watched(dst);
        self.log_list_pop(src, from_left);
        self.notify_list_event(src, from_left, true);
        self.notify_list_event(dst, to_left, false);
    }

    /// One `LPOP` / `RPOP` effect record. The pushed half is logged by
    /// [`Self::log_list_push`] on whichever shard actually took the element.
    pub(crate) fn log_list_pop(&mut self, key: &[u8], from_left: bool) {
        if self.aof.is_some() {
            let mut c = kevy_resp::Argv::with_capacity(2, 0);
            c.push(if from_left { b"LPOP" } else { b"RPOP" });
            c.push(key);
            self.log(&c);
        }
    }

    /// One `LPUSH` / `RPUSH` effect record carrying the moved element.
    pub(crate) fn log_list_push(&mut self, key: &[u8], value: &[u8], to_left: bool) {
        if self.aof.is_some() {
            let mut c = kevy_resp::Argv::with_capacity(3, 0);
            c.push(if to_left { b"LPUSH" } else { b"RPUSH" });
            c.push(key);
            c.push(value);
            self.log(&c);
        }
    }

    /// `lpop` / `rpop` / `lpush` / `rpush` keyspace events, matching the
    /// names Redis fires for the same effects.
    pub(crate) fn notify_list_event(&mut self, key: &[u8], left: bool, popped: bool) {
        if self.notify_flags.is_empty() || !self.notify_flags.list {
            return;
        }
        let event: &[u8] = match (popped, left) {
            (true, true) => b"lpop",
            (true, false) => b"rpop",
            (false, true) => b"lpush",
            (false, false) => b"rpush",
        };
        self.notify_keyspace_event(event, key);
    }
}
