//! v2.2 — zset/set algebra `*STORE` orchestrator, step-1→step-2
//! transition (same two-hop shape as [`crate::exec_rename`]).
//!
//! Step 1 (built by `build_zalgebra_store`): scored/set gathers fan
//! out to the source keys' shards, folding into
//! [`Agg::ZStoreGather`]. When the last gather lands, `fold` routes
//! the agg here: the origin computes the combination via the shared
//! `kevy_store` pure algebra, re-arms the slot as a plain
//! `Agg::SumInt`, and ships `Op::ZStoreResult` / `Op::SetStoreResult`
//! to `dst`'s owning shard (or executes locally). Step 2's
//! `Part::Int(cardinality)` folds through the SumInt into the `:n`
//! reply — no dedicated finalize needed.

use std::collections::HashMap;

/// One source key's scored members in request order.
type ScoredInput = Vec<(Vec<u8>, f64)>;

use crate::Commands;
use crate::message::{Agg, Gathered, Inbound, Op, SmallReply, ZCombine};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    pub(crate) fn finalize_zstore_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::ZStoreGather {
            combine,
            weights,
            aggregate,
            dst,
            keys,
            got,
        } = agg
        else {
            return;
        };
        // Rebuild inputs in request-key order; WRONGTYPE on any source
        // aborts with the standard error (Redis behavior).
        let (zset_inputs, wrongtype) = collect_scored(&keys, &got);
        if wrongtype {
            self.fill_zstore_slot(
                conn_id,
                seq,
                b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec(),
            );
            return;
        }
        let dst_shard = self.shard_of(&dst);
        let op = match combine {
            ZCombine::ZInter | ZCombine::ZUnion | ZCombine::ZDiff => {
                let pairs = match combine {
                    ZCombine::ZInter => {
                        kevy_store::zinter(&zset_inputs, weights.as_deref(), aggregate)
                    }
                    ZCombine::ZUnion => {
                        kevy_store::zunion(&zset_inputs, weights.as_deref(), aggregate)
                    }
                    _ => kevy_store::zdiff(&zset_inputs),
                };
                Op::ZStoreResult { dst, pairs }
            }
            ZCombine::SInter | ZCombine::SUnion | ZCombine::SDiff => {
                // Set forms ride the Members gather: inputs carry score
                // 1.0 placeholders only when sources were sets; strip.
                let sets: Vec<Vec<Vec<u8>>> = zset_inputs
                    .into_iter()
                    .map(|inp| inp.into_iter().map(|(m, _)| m).collect())
                    .collect();
                let members = match combine {
                    ZCombine::SInter => crate::reduce::set_intersect(&sets),
                    ZCombine::SUnion => crate::reduce::set_union(&sets),
                    _ => crate::reduce::set_diff(&sets),
                };
                Op::SetStoreResult { dst, members }
            }
        };
        // Re-arm the slot for step 2: one Int part sums into the reply.
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::SumInt(0);
            }
        }
        if dst_shard == self.id {
            let part = self.exec_op(op);
            self.fold(conn_id, seq, part);
        } else {
            self.send_to(
                dst_shard,
                Inbound::Request {
                    origin: self.id,
                    conn: conn_id,
                    seq,
                    op,
                },
            );
        }
    }

    /// Complete the pending slot with a pre-encoded reply (parse /
    /// WRONGTYPE aborts).
    fn fill_zstore_slot(&mut self, conn_id: u64, seq: u64, reply: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::First(None);
            }
        }
        self.fold(conn_id, seq, crate::message::Part::Reply(SmallReply::from_vec(reply)));
    }
}

/// Inputs in request order: `Scored` as-is, set `Members` at 1.0,
/// missing = empty. Second return = a WRONGTYPE was gathered.
fn collect_scored(
    keys: &[Vec<u8>],
    got: &HashMap<Vec<u8>, Gathered>,
) -> (Vec<ScoredInput>, bool) {
    let mut inputs = Vec::with_capacity(keys.len());
    for k in keys {
        match got.get(k) {
            Some(Gathered::Scored(p)) => inputs.push(p.clone()),
            Some(Gathered::Members(m)) => {
                inputs.push(m.iter().map(|v| (v.clone(), 1.0)).collect());
            }
            Some(Gathered::WrongType) => return (inputs, true),
            _ => inputs.push(Vec::new()),
        }
    }
    (inputs, false)
}
