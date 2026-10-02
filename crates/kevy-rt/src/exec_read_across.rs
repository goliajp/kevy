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

/// Which arguments a [`crate::Route::ReadAcross`] /
/// [`crate::Route::StoreFromCopies`] reads, and the one it writes.
pub(crate) struct Across {
    pub(crate) keys: Range<usize>,
    pub(crate) dst: Option<usize>,
}

impl<C: Commands> Shard<C> {
    pub(crate) fn start_read_across<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        across: Across,
        is_quit: bool,
        cluster_conn: bool,
    ) {
        if cluster_conn {
            self.push_pending_single(conn_id, is_quit);
            let e = b"-CROSSSLOT Keys in request don't hash to the same slot\r\n";
            return self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(e)));
        }
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for i in across.keys {
            by_shard.entry(self.shard_of(&args[i])).or_default().push(args[i].to_vec());
        }
        let dst = across.dst.map(|i| args[i].to_vec());
        let agg = Agg::ReadAcross { argv: args.to_argv(), got: HashMap::new(), dst };
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
        dst: Option<Vec<u8>>,
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
        // the run over copies is no write of this shard's: nothing to record
        crate::propagation::discard_override();
        if let Some(dst) = dst
            && out.first() != Some(&b'-')
        {
            return self.place_result(conn_id, seq, argv, dst, &mut copies, out);
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.done = Some(SmallReply::from_vec(out));
            }
            drain_front(c);
        }
    }

    /// Carry the result's value to `dst`'s shard — or remove `dst` when
    /// there is none — and reply once it is there.
    fn place_result(
        &mut self,
        conn_id: u64,
        seq: u64,
        argv: &kevy_resp::Argv,
        dst: Vec<u8>,
        copies: &mut kevy_store::Store,
        reply: Vec<u8>,
    ) {
        let op = match copies.clone_with_ttl(&dst) {
            Some((value, _)) => Op::StoreValue {
                key: dst.clone(),
                value,
                event: self.commands.placed_event(argv),
                class: self.commands.notify_class(argv),
                keep_ttl: self.commands.placed_keeps_ttl(argv),
            },
            None => Op::Del(vec![dst.clone()]),
        };
        if let Some(slot) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|c| c.pending.get_mut((seq - c.next_emit) as usize))
        {
            slot.remaining = 1;
            slot.agg = Some(Box::new(Agg::First(Some(SmallReply::from_vec(reply)))));
        }
        let shard = self.shard_of(&dst);
        if shard == self.id {
            self.exec_local(conn_id, seq, op);
        } else {
            self.send_to(shard, Inbound::Request { origin: self.id, conn: conn_id, seq, op });
        }
    }

    /// `Op::StoreValue` on the key's own shard.
    pub(crate) fn op_store_value(
        &mut self,
        key: Vec<u8>,
        value: kevy_store::Value,
        event: &[u8],
        class: Option<crate::NotifyKind>,
        keep_ttl: bool,
    ) -> Part {
        let ttl_ms = keep_ttl.then(|| self.store.pttl(&key)).and_then(|ms| u64::try_from(ms).ok());
        self.log_value_placed(&key, &value, ttl_ms);
        if keep_ttl {
            self.store.put_keep_ttl(key.clone(), value);
        } else {
            self.store.put_with_ttl(key.clone(), value, None);
        }
        self.note_key_mutated(&key);
        if let Some(class) = class {
            self.notify_class_event(class, event, &key);
        }
        Part::Ok
    }
}
