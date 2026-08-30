//! The `SCAN` cursor orchestrator — wire-cursor encoding and the
//! per-call shard paging that makes kevy's SCAN a real bounded-work,
//! rehash-tolerant iterator (v4).
//!
//! Wire cursor layout (u64, decimal on the wire like Redis):
//!
//! ```text
//! bits 63..54  shard index   (SCAN_SHARD_BITS = 10 → up to 1024 shards)
//! bits 53..0   in-shard reverse-binary bucket-group cursor
//!              (SCAN_POS_BITS = 54 → per-shard tables up to 2^54
//!               bucket-groups = 2^58 buckets)
//! ```
//!
//! Cursor `0` starts shard 0; when a shard's walk completes the cursor
//! advances to `(shard + 1, 0)`; after the last shard the reply cursor
//! is `0`. Consequence (documented deviation): cursors are only valid
//! on the server — and shard count — that issued them, the same way
//! Redis Cluster cursors are per-node. A cursor whose shard bits exceed
//! this server's shard count terminates the sweep (cursor 0, empty
//! batch) instead of erroring, mirroring Redis's mask-to-table
//! tolerance of stale cursors.

use crate::Commands;
use crate::message::{Agg, Op, Part, SmallReply};
use crate::route::ScanArgs;
use crate::shard::Shard;
use kevy_resp::{encode_array_len, encode_bulk};

/// Bits of the wire cursor carrying the in-shard position.
pub(crate) const SCAN_POS_BITS: u32 = 54;
/// Mask of the in-shard position bits.
pub(crate) const SCAN_POS_MASK: u64 = (1 << SCAN_POS_BITS) - 1;

/// Compose a wire cursor from `(shard, in-shard position)`.
fn wire_cursor(shard: usize, pos: u64) -> u64 {
    ((shard as u64) << SCAN_POS_BITS) | (pos & SCAN_POS_MASK)
}

/// Encode the `[cursor, [keys]]` SCAN reply.
fn scan_reply(cursor: u64, keys: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + keys.iter().map(|k| k.len() + 16).sum::<usize>());
    encode_array_len(&mut out, 2);
    encode_bulk(&mut out, cursor.to_string().as_bytes());
    encode_array_len(&mut out, keys.len() as i64);
    for k in keys {
        encode_bulk(&mut out, k);
    }
    out
}

impl<C: Commands> Shard<C> {
    /// Build the (single) target for a `Route::Scan`: ONE `Op::ScanStep`
    /// against the cursor's shard. Parse errors (`Err`) and cursors
    /// addressing a shard this server doesn't have resolve immediately
    /// via a pre-baked `Agg::First` reply.
    pub(crate) fn build_scan_targets(
        &self,
        spec: Result<ScanArgs, &'static str>,
    ) -> (Vec<(usize, Op)>, Agg) {
        let args = match spec {
            Ok(a) => a,
            Err(msg) => {
                let mut out = Vec::new();
                kevy_resp::encode_error(&mut out, msg);
                return (Vec::new(), Agg::First(Some(SmallReply::from_vec(out))));
            }
        };
        let shard = (args.cursor >> SCAN_POS_BITS) as usize;
        let pos = args.cursor & SCAN_POS_MASK;
        if shard >= self.nshards {
            // Stale cursor from a different shard layout — the sweep is
            // over (Redis likewise masks stale cursors rather than erroring).
            let reply = scan_reply(0, &[]);
            return (Vec::new(), Agg::First(Some(SmallReply::from_vec(reply))));
        }
        let op = Op::ScanStep {
            cursor: pos,
            count: args.count,
            pattern: args.pattern.clone(),
            type_filter: args.type_filter.clone(),
        };
        (
            vec![(shard, op)],
            Agg::ScanPage {
                shard,
                budget: args.count.max(1),
                pattern: args.pattern,
                type_filter: args.type_filter,
                keys: Vec::new(),
                next: 0,
            },
        )
    }

    /// Complete (or chain) a `SCAN` slot once its in-flight page folded.
    /// Reply cases:
    /// - shard has more (`next != 0`) → `[(shard, next), keys]`
    /// - shard exhausted, budget left, more shards → re-arm + chain into
    ///   `shard + 1` (this is what lets an empty server answer cursor 0
    ///   in one call)
    /// - shard exhausted, budget spent, more shards → `[(shard+1, 0), keys]`
    /// - last shard exhausted → `[0, keys]`
    pub(crate) fn finalize_scan_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::ScanPage { shard, budget, pattern, type_filter, keys, next } = agg else {
            return;
        };
        if next != 0 {
            return self.fill_scan_slot(conn_id, seq, scan_reply(wire_cursor(shard, next), &keys));
        }
        let next_shard = shard + 1;
        if next_shard >= self.nshards {
            return self.fill_scan_slot(conn_id, seq, scan_reply(0, &keys));
        }
        if budget == 0 {
            return self.fill_scan_slot(
                conn_id,
                seq,
                scan_reply(wire_cursor(next_shard, 0), &keys),
            );
        }
        // Budget left and shards remain: chain the sweep into the next
        // shard within this same client call.
        let op = Op::ScanStep {
            cursor: 0,
            count: budget,
            pattern: pattern.clone(),
            type_filter: type_filter.clone(),
        };
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::ScanPage {
                    shard: next_shard,
                    budget,
                    pattern,
                    type_filter,
                    keys,
                    next: 0,
                };
            }
        }
        self.dispatch_targets(conn_id, seq, vec![(next_shard, op)]);
    }

    /// Complete the pending SCAN slot with a pre-encoded reply (mirrors
    /// `fill_extension_slot`'s re-arm-then-fold shape).
    fn fill_scan_slot(&mut self, conn_id: u64, seq: u64, reply: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = 1;
                slot.agg = Agg::First(None);
            }
        }
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(reply)));
    }
}
