//! The second round of a cross-shard `XREADGROUP`: once every stream's
//! check is back, either answer the first refusal or ship the reads.

use crate::Commands;
use crate::message::{Agg, Op};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Answer the first refusal in request order, or re-arm the slot as
    /// an [`Agg::XReadGather`] and ship `reads`.
    pub(crate) fn finalize_xread_check(
        &mut self,
        conn_id: u64,
        seq: u64,
        refusals: Vec<Option<Vec<u8>>>,
        reads: Vec<(usize, Op)>,
    ) {
        if let Some(refusal) = refusals.into_iter().flatten().next() {
            self.fill_zstore_slot(conn_id, seq, refusal);
            return;
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = reads.len() as u32;
                slot.agg = crate::message_agg::slot_agg(Agg::XReadGather {
                    slots: vec![None; reads.len()],
                });
            }
        }
        self.dispatch_targets(conn_id, seq, reads);
    }
}
