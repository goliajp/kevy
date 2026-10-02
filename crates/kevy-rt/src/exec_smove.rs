//! `SMOVE` with its two keys on different shards ([`crate::Route::SetMove`]):
//! the destination's type first, then the source's checks and the
//! removal, then the add — Redis's order of checks, kept across shards.
//! The move is not atomic: between the removal and the add the member is
//! in neither set. Co-locate the keys with a `{hashtag}` for Redis's
//! atomic form.

use kevy_resp::ArgvView;

use crate::Commands;
use crate::message::{Agg, Inbound, Op, Part, SmallReply};
use crate::reduce::drain_front;
use crate::shard::Shard;

const WRONGTYPE: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";

impl<C: Commands> Shard<C> {
    pub(crate) fn start_set_move<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        is_quit: bool,
        cluster_conn: bool,
    ) {
        if cluster_conn {
            self.push_pending_single(conn_id, is_quit);
            let e = b"-CROSSSLOT Keys in request don't hash to the same slot\r\n";
            return self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(e)));
        }
        let (src, dst, member) = (args[1].to_vec(), args[2].to_vec(), args[3].to_vec());
        let shard = self.shard_of(&dst);
        let agg = Agg::SetMove { step: 1, src, dst: dst.clone(), member, answer: 0 };
        self.push_pending_slot(conn_id, 1, agg, is_quit);
        self.set_move_send(conn_id, seq, shard, Op::SetMoveCheck(dst));
    }

    fn set_move_send(&mut self, conn_id: u64, seq: u64, shard: usize, op: Op) {
        if shard == self.id {
            self.exec_local(conn_id, seq, op);
        } else {
            self.send_to(shard, Inbound::Request { origin: self.id, conn: conn_id, seq, op });
        }
    }

    /// One step has answered: the next one, or the reply.
    pub(crate) fn finalize_set_move(
        &mut self,
        conn_id: u64,
        seq: u64,
        step: u8,
        (src, dst, member): (Vec<u8>, Vec<u8>, Vec<u8>),
        answer: i64,
    ) {
        let (next, reply) = self.set_move_next(step, answer, (&src, &dst, &member));
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        let idx = (seq - c.next_emit) as usize;
        let Some(slot) = c.pending.get_mut(idx) else { return };
        match next {
            Some((shard, op)) => {
                slot.remaining = 1;
                slot.agg =
                    Some(Box::new(Agg::SetMove { step: step + 1, src, dst, member, answer: 0 }));
                self.set_move_send(conn_id, seq, shard, op);
            }
            None => {
                slot.done = Some(SmallReply::from_slice(reply));
                drain_front(c);
            }
        }
    }

    /// The step after `step` answered `answer`, or the reply.
    fn set_move_next(
        &self,
        step: u8,
        answer: i64,
        (src, dst, member): (&[u8], &[u8], &[u8]),
    ) -> (Option<(usize, Op)>, &'static [u8]) {
        let put = |key: &[u8]| Op::SetMovePut { key: key.to_vec(), member: member.to_vec() };
        match (step, answer) {
            (1, ok) => {
                let take = Op::SetMoveTake {
                    src: src.to_vec(),
                    member: member.to_vec(),
                    dst_is_set: ok == 1,
                };
                (Some((self.shard_of(src), take)), b"")
            }
            (2, 1) => (Some((self.shard_of(dst), put(dst))), b""),
            (2, 0) => (None, b":0\r\n"),
            (3, 1) => (None, b":1\r\n"),
            // the destination stopped being a set: the member goes back
            (3, _) => (Some((self.shard_of(src), put(src))), b""),
            _ => (None, WRONGTYPE),
        }
    }

    /// Step 2: a missing source answers 0 whatever the destination is; a
    /// source or destination that is not a set answers WRONGTYPE; else the
    /// member is removed, if it is there.
    pub(crate) fn op_set_move_take(&mut self, src: &[u8], member: &[u8], dst_is_set: bool) -> Part {
        match self.store.scard(src) {
            Ok(0) => return Part::Int(0),
            Err(_) => return Part::Int(-1),
            Ok(_) if !dst_is_set => return Part::Int(-1),
            Ok(_) => {}
        }
        match self.store.srem(src, &[member]) {
            Ok(1) => {
                self.note_key_mutated(src);
                self.log_set_change(b"SREM", src, member);
                self.notify_class_event(crate::NotifyKind::Set, b"srem", src);
                if self.notify_flags.is_active() {
                    self.drain_store_notify();
                }
                Part::Int(1)
            }
            _ => Part::Int(0),
        }
    }

    /// Step 3, and its undo on the source: add the member.
    pub(crate) fn op_set_move_put(&mut self, key: &[u8], member: &[u8]) -> Part {
        match self.store.sadd(key, &[member]) {
            Ok(n) => {
                if n > 0 {
                    self.note_key_mutated(key);
                    self.log_set_change(b"SADD", key, member);
                    self.notify_class_event(crate::NotifyKind::Set, b"sadd", key);
                }
                Part::Int(1)
            }
            Err(_) => Part::Int(-1),
        }
    }

    fn log_set_change(&mut self, verb: &[u8], key: &[u8], member: &[u8]) {
        if self.aof.is_some() || self.replicate.is_some() {
            let mut c = kevy_resp::Argv::with_capacity(3, 0);
            c.push(verb);
            c.push(key);
            c.push(member);
            self.log_effect(&c);
        }
    }
}
