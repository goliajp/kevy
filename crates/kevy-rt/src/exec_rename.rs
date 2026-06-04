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

use crate::message::{Agg, Inbound, Op, Part, PendingSlot};
use crate::reduce::shard_of;
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
        let src_shard = shard_of(&src, self.nshards);
        let dst_shard = shard_of(&dst, self.nshards);

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

        // Cross-shard: v2-3a placeholder. The orchestrator (Take from
        // src shard, then Put to dst shard, then reply) lands in v2-3b.
        // Redis cluster mode returns `CROSSSLOT` here; kevy uses a
        // `CROSSSHARD` prefix to make it clear this isn't a cluster
        // semantics issue but a not-yet-implemented optimisation.
        self.fold_rename_reply(
            conn_id,
            seq,
            b"-CROSSSHARD source and destination keys are on different shards (cross-shard RENAME pending v2-3b)\r\n".to_vec(),
        );
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
        self.fold(conn_id, seq, Part::Reply(reply));
    }
}
