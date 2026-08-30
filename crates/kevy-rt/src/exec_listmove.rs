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

use crate::Commands;
use crate::message::Part;
use crate::message::{Agg, Inbound, Op, PendingSlot, SmallReply};
use crate::message_agg::ListMoveStep;
use crate::reduce::drain_front;
use crate::shard::Shard;
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

        self.start_list_move_xshard(
            conn_id, seq, src, dst, src_shard, dst_shard, from_left, to_left, blocking,
        );
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
            step,
            blocking,
            src,
            dst,
            src_shard,
            dst_shard,
            from_left,
            to_left,
            taken,
            pushed,
        } = agg
        else {
            return;
        };
        let m = Move { conn_id, seq, blocking, src, dst, src_shard, dst_shard, from_left, to_left };
        match step {
            ListMoveStep::Take => self.after_take(m, taken),
            ListMoveStep::Push => self.after_push(m, taken, pushed),
            // The element is back on the source; the client gets the error the
            // destination raised.
            ListMoveStep::Restore => {
                self.finish_list_move(m.conn_id, m.blocking, &m.src, wrongtype())
            }
        }
    }

    /// Step 1 landed: the source either gave up an element or it did not.
    fn after_take(&mut self, m: Move, taken: Option<Result<Option<Vec<u8>>, ()>>) {
        let value = match taken {
            // Source empty or absent. A plain move replies nil. A parked
            // BRPOPLPUSH means another client drained the source between the
            // readiness signal and our Take — hand an EMPTY reply back to the
            // arbiter, which re-arms the watchers and keeps the conn parked.
            None | Some(Ok(None)) => {
                let miss = if m.blocking { Vec::new() } else { b"$-1\r\n".to_vec() };
                return self.finish_list_move(m.conn_id, m.blocking, &m.src, miss);
            }
            Some(Err(())) => {
                return self.finish_list_move(m.conn_id, m.blocking, &m.src, wrongtype());
            }
            Some(Ok(Some(v))) => v,
        };
        self.rearm(m.conn_id, m.agg(ListMoveStep::Push, Some(value.clone()), None));
        let (conn_id, seq, dst_shard, dst, to_left) =
            (m.conn_id, m.seq, m.dst_shard, m.dst.clone(), m.to_left);
        self.dispatch_op(conn_id, seq, dst_shard, Op::ListMovePush { key: dst, value, to_left });
    }

    /// Step 2 landed: the destination took the element, or refused it.
    fn after_push(
        &mut self,
        m: Move,
        taken: Option<Result<Option<Vec<u8>>, ()>>,
        pushed: Option<bool>,
    ) {
        let Some(Ok(Some(element))) = taken else {
            // Unreachable by construction — we only enter Push holding an
            // element. Reply nil rather than panic inside a reactor.
            return self.finish_list_move(m.conn_id, m.blocking, &m.src, b"$-1\r\n".to_vec());
        };
        if pushed == Some(true) {
            let mut out = Vec::with_capacity(element.len() + 16);
            kevy_resp::encode_bulk(&mut out, &element);
            return self.finish_list_move(m.conn_id, m.blocking, &m.src, out);
        }
        // The destination exists and is not a list. Put the element back where
        // it came from before telling the client — a WRONGTYPE must not cost
        // them their data.
        self.rearm(m.conn_id, m.agg(ListMoveStep::Restore, Some(element.clone()), Some(false)));
        let (conn_id, seq, src_shard, src, from_left) =
            (m.conn_id, m.seq, m.src_shard, m.src.clone(), m.from_left);
        self.dispatch_op(
            conn_id,
            seq,
            src_shard,
            Op::ListMoveRestore { key: src, value: element, from_left },
        );
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

/// The orchestrator's invariants, carried between steps so each step's handler
/// takes one argument instead of nine.
struct Move {
    conn_id: u64,
    seq: u64,
    blocking: bool,
    src: Vec<u8>,
    dst: Vec<u8>,
    src_shard: usize,
    dst_shard: usize,
    from_left: bool,
    to_left: bool,
}

impl Move {
    /// Rebuild the agg for the next step, carrying the element forward.
    fn agg(&self, step: ListMoveStep, taken: Option<Vec<u8>>, pushed: Option<bool>) -> Agg {
        Agg::ListMoveOrchestrator {
            step,
            blocking: self.blocking,
            src: self.src.clone(),
            dst: self.dst.clone(),
            src_shard: self.src_shard,
            dst_shard: self.dst_shard,
            from_left: self.from_left,
            to_left: self.to_left,
            taken: taken.map(|v| Ok(Some(v))),
            pushed,
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
    pub(crate) fn after_list_move(
        &mut self,
        src: &[u8],
        dst: &[u8],
        from_left: bool,
        to_left: bool,
    ) {
        self.note_key_mutated(src);
        self.note_key_mutated(dst);
        self.log_list_pop(src, from_left);
        self.notify_list_event(src, from_left, true);
        self.notify_list_event(dst, to_left, false);
    }

    /// One `LPOP` / `RPOP` effect record. The pushed half is logged by
    /// [`Self::log_list_push`] on whichever shard actually took the element.
    pub(crate) fn log_list_pop(&mut self, key: &[u8], from_left: bool) {
        // Built whenever there is anywhere for it to go. Gating on the
        // AOF alone meant a replication-only deployment (AOF off) never
        // produced the record at all, so the move reached neither disk
        // nor replica.
        if self.aof.is_some() || self.replicate.is_some() {
            let mut c = kevy_resp::Argv::with_capacity(2, 0);
            c.push(if from_left { b"LPOP" } else { b"RPOP" });
            c.push(key);
            self.log_effect(&c);
        }
    }

    /// One `LPUSH` / `RPUSH` effect record carrying the moved element.
    pub(crate) fn log_list_push(&mut self, key: &[u8], value: &[u8], to_left: bool) {
        if self.aof.is_some() || self.replicate.is_some() {
            let mut c = kevy_resp::Argv::with_capacity(3, 0);
            c.push(if to_left { b"LPUSH" } else { b"RPUSH" });
            c.push(key);
            c.push(value);
            self.log_effect(&c);
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

    /// `Op::ListMoveTake` — split out of `exec_op` for the LOC ceiling.
    pub(crate) fn op_list_move_take(&mut self, key: &[u8], from_left: bool) -> Part {
        // Step 1 of the cross-shard move: pop one element. The
        // destination is not touched until the element is in hand, so
        // an empty source costs the destination nothing.
        let popped = if from_left { self.store.lpop(key, 1) } else { self.store.rpop(key, 1) };
        match popped {
            Ok(mut v) => {
                let element = v.pop();
                if element.is_some() {
                    self.note_key_mutated(key);
                    self.log_list_pop(key, from_left);
                    self.notify_list_event(key, from_left, true);
                }
                Part::ListMoveTaken(Ok(element))
            }
            Err(_) => Part::ListMoveTaken(Err(())),
        }
    }

    /// `Op::ListMovePush` — split out of `exec_op` for the LOC ceiling.
    pub(crate) fn op_list_move_push(&mut self, key: &[u8], value: Vec<u8>, to_left: bool) -> Part {
        // Step 2. A destination that exists and is not a list refuses
        // the element and hands it back — the orchestrator restores it
        // to the source rather than dropping it.
        let pushed = if to_left {
            self.store.lpush(key, &[value.as_slice()])
        } else {
            self.store.rpush(key, &[value.as_slice()])
        };
        match pushed {
            Ok(_) => {
                self.note_key_mutated(key);
                self.notify_list_event(key, to_left, false);
                self.log_list_push(key, &value, to_left);
                Part::ListMovePushed { refused: None }
            }
            Err(_) => Part::ListMovePushed { refused: Some(value) },
        }
    }

    /// `Op::ListMoveRestore` — split out of `exec_op` for the LOC ceiling.
    pub(crate) fn op_list_move_restore(
        &mut self,
        key: &[u8],
        value: &[u8],
        from_left: bool,
    ) -> Part {
        // Rollback: put the element back on the end it was taken from.
        let _ = if from_left {
            self.store.lpush(key, &[value])
        } else {
            self.store.rpush(key, &[value])
        };
        self.note_key_mutated(key);
        self.log_list_push(key, value, from_left);
        Part::Ok
    }
}
