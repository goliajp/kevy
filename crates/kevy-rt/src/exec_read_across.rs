//! [`crate::Route::ReadAcross`] with its keys on more than one shard:
//! each shard hands back copies of the keys it owns, and the command runs
//! over the copies on the shard it came to, in a store made for it, so a
//! read spanning shards answers exactly as it would on one.

use std::collections::HashMap;
use std::ops::Range;

use kevy_resp::ArgvView;

use crate::Commands;
use crate::message::{Agg, Inbound, Op, Part, SmallReply};
use crate::message_kinds::{GatherKind, Gathered};
use crate::reduce::drain_front;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    pub(crate) fn start_read_across<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        keys: Range<usize>,
        is_quit: bool,
        cluster_conn: bool,
    ) {
        if cluster_conn {
            self.push_pending_single(conn_id, is_quit);
            let e = b"-CROSSSLOT Keys in request don't hash to the same slot\r\n";
            return self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(e)));
        }
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for i in keys {
            by_shard.entry(self.shard_of(&args[i])).or_default().push(args[i].to_vec());
        }
        let agg = Agg::ReadAcross { argv: args.to_argv(), got: HashMap::new() };
        self.push_pending_slot(conn_id, by_shard.len() as u32, agg, is_quit);
        for (shard, keys) in by_shard {
            let op = Op::Gather(GatherKind::Value, keys);
            if shard == self.id {
                self.exec_local(conn_id, seq, op);
            } else {
                self.send_to(shard, Inbound::Request { origin: self.id, conn: conn_id, seq, op });
            }
        }
    }

    /// Every copy is in: run the command over them.
    pub(crate) fn finalize_read_across(
        &mut self,
        conn_id: u64,
        seq: u64,
        argv: &kevy_resp::Argv,
        got: HashMap<Vec<u8>, Gathered>,
    ) {
        let mut copies = kevy_store::Store::new();
        for (key, g) in got {
            if let Gathered::Value(Some((value, ttl_ms))) = g {
                copies.put_with_ttl(key, value, ttl_ms);
            }
        }
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        let idx = (seq - c.next_emit) as usize;
        let proto = c.pending.get(idx).map_or(kevy_resp::RespVersion::V2, |s| s.proto);
        let mut out = Vec::new();
        let meta = crate::message::DispatchMeta {
            is_write: false,
            wake_idx: None,
            key_idx: None,
            verb: crate::VerbId::UNKNOWN,
            key_hash: 0,
        };
        crate::exec_dispatch::dispatch_proto(
            &self.commands,
            &mut copies,
            argv,
            proto,
            meta,
            &mut out,
        );
        if let Some(c) = self.conns.get_mut(&conn_id) {
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.done = Some(SmallReply::from_vec(out));
            }
            drain_front(c);
        }
    }
}
