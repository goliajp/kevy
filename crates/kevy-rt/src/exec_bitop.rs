//! `BITOP op dst src [src …]` across shards.
//!
//! Three shard-crossings in one command: the sources are read where
//! they live, the bytes are combined on the shard that took the
//! command, and the result is written where the destination lives. Left
//! to the router's catch-all the whole thing would run on `args[1]`'s
//! shard — and `args[1]` is the OPERATOR, not a key, so the command
//! would land on whichever shard the word "AND" hashes to.
//!
//! The byte arithmetic itself is `kevy_store::bitop_combine`, shared
//! with the embedded facade. It used to live in `kevy-embedded`, where
//! this crate could not reach it: a sibling. Copying it here would have
//! duplicated the padding rules — the 0xff tail of NOT among them — in
//! two places that no test compares.

use std::collections::HashMap;

use crate::Commands;
use crate::message::{Agg, Gathered, Inbound, Op, Part, SmallReply};
use crate::message_kinds::GatherKind;
use crate::reduce::drain_front;
use crate::shard::Shard;
use kevy_resp::ArgvView;
use kevy_store::BitOp;

/// A well-formed `BITOP <AND|OR|XOR|NOT> dst src [src …]`.
pub(crate) struct BitOpCall {
    op: BitOp,
    dst: Vec<u8>,
    keys: Vec<Vec<u8>>,
}

/// `BITOP` parsed out of argv, or the wire error Redis answers.
pub(crate) fn parse_bitop<A: ArgvView + ?Sized>(args: &A) -> Result<BitOpCall, &'static str> {
    if args.len() < 4 {
        return Err("-ERR wrong number of arguments for 'bitop' command\r\n");
    }
    let op = match args[1].to_ascii_uppercase().as_slice() {
        b"AND" => BitOp::And,
        b"OR" => BitOp::Or,
        b"XOR" => BitOp::Xor,
        b"NOT" => BitOp::Not,
        _ => return Err("-ERR syntax error\r\n"),
    };
    let srcs: Vec<Vec<u8>> = (3..args.len()).map(|i| args[i].to_vec()).collect();
    if op == BitOp::Not && srcs.len() != 1 {
        return Err("-ERR BITOP NOT must be called with a single source key.\r\n");
    }
    Ok(BitOpCall { op, dst: args[2].to_vec(), keys: srcs })
}

impl<C: Commands> Shard<C> {
    /// Entry point for [`crate::Route::BitOpStore`]: one gather per
    /// shard that owns at least one source.
    pub(crate) fn start_bitop<A: ArgvView + ?Sized>(&mut self, conn_id: u64, seq: u64, args: &A) {
        let BitOpCall { op, dst, keys } = match parse_bitop(args) {
            Ok(t) => t,
            Err(e) => return self.fold_bitop_reply(conn_id, seq, e.as_bytes().to_vec()),
        };
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for k in &keys {
            by_shard.entry(self.shard_of(k)).or_default().push(k.clone());
        }
        let targets: Vec<(usize, Op)> = by_shard
            .into_iter()
            .map(|(s, ks)| (s, Op::Gather(GatherKind::StrStrict, ks)))
            .collect();
        let agg = Agg::BitOpGather { op, dst, keys, got: HashMap::new() };
        self.push_pending_slot(conn_id, targets.len() as u32, agg, false);
        for (shard, op) in targets {
            if shard == self.id {
                let part = self.exec_op(op);
                self.fold(conn_id, seq, part);
            } else {
                let origin = self.id;
                self.send_to(shard, Inbound::Request { origin, conn: conn_id, seq, op });
            }
        }
    }

    /// Write what the combine produced, or delete the destination when
    /// it produced nothing — Redis removes the key rather than storing
    /// a zero-length string, and the reply is the stored length either
    /// way. Both branches are logged, and the DEL half is the one worth
    /// naming: logging only the SET leaves the destination to come back
    /// from the log after a restart, a key Redis had removed.
    pub(crate) fn op_bitop_result(&mut self, key: Vec<u8>, value: &[u8]) -> Part {
        let len = value.len() as i64;
        let mut argv = kevy_resp::Argv::default();
        if value.is_empty() {
            self.store.del(&[&key[..]]);
            argv.push(b"DEL");
            argv.push(&key);
        } else {
            self.store.set_slice(&key, value, None, false, false);
            argv.push(b"SET");
            argv.push(&key);
            argv.push(value);
        }
        self.note_key_mutated(&key);
        // `log_effect`, not `log_write`: the first writes the AOF, the
        // second also pushes the mutation to replicas and therefore to
        // the change feed that reads their backlog. `propgate` caught
        // this one — a BITOP result that was durable and unreplicated,
        // which is the exact shape of the three data-loss bugs that
        // gate was written after.
        self.log_effect(&argv);
        Part::Int(len)
    }

    /// Every source has answered: combine in ARGV order and ship the
    /// result to the destination's shard.
    pub(crate) fn finalize_bitop_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::BitOpGather { op, dst, keys, mut got } = agg else { return };
        let mut srcs: Vec<Vec<u8>> = Vec::with_capacity(keys.len());
        for k in &keys {
            match got.remove(k) {
                // Redis refuses the whole command when any source holds
                // something other than a string — where MGET, gathering
                // the same way, answers nil for it.
                Some(Gathered::WrongType) => {
                    let e =
                        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
                    return self.fill_bitop_slot(conn_id, seq, e.to_vec());
                }
                Some(Gathered::Str(v)) => srcs.push(v.unwrap_or_default()),
                // A key nobody gathered is a key nobody holds.
                _ => srcs.push(Vec::new()),
            }
        }
        let max_len = srcs.iter().map(Vec::len).max().unwrap_or(0);
        let value =
            if max_len == 0 { Vec::new() } else { kevy_store::bitop_combine(op, &srcs, max_len) };
        let dst_shard = self.shard_of(&dst);
        self.rearm_bitop_slot(conn_id, seq);
        let put = Op::BitOpResult { key: dst, value };
        if dst_shard == self.id {
            let part = self.exec_op(put);
            self.fold(conn_id, seq, part);
        } else {
            let origin = self.id;
            self.send_to(dst_shard, Inbound::Request { origin, conn: conn_id, seq, op: put });
        }
    }

    /// Step 2 folds through a fresh `SumInt`, which turns the stored
    /// length into `:N` — the same trick the geo `*STORE` family uses.
    fn rearm_bitop_slot(&mut self, conn_id: u64, seq: u64) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::SumInt(0);
            }
        }
    }

    fn fill_bitop_slot(&mut self, conn_id: u64, seq: u64, bytes: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.done = Some(SmallReply::from_vec(bytes));
            }
            drain_front(c);
        }
    }

    fn fold_bitop_reply(&mut self, conn_id: u64, seq: u64, reply: Vec<u8>) {
        self.push_pending_slot(conn_id, 1, Agg::First(None), false);
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(reply)));
    }
}
