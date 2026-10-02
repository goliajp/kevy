//! `MSETNX` with its keys on more than one shard ([`crate::Route::MSetNx`]):
//! every shard is asked whether any of its keys exists, and only when none
//! does are the pairs set, shard by shard. Between the question and the
//! sets another client may create one of the keys; co-locate them with a
//! `{hashtag}` for Redis's atomic form.

use std::collections::HashMap;

use kevy_resp::ArgvView;

use crate::Commands;
use crate::message::{Agg, Inbound, KvPairs, Op, Part, SmallReply};
use crate::reduce::drain_front;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    pub(crate) fn start_msetnx<A: ArgvView + ?Sized>(
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
        let mut by_shard: HashMap<usize, KvPairs> = HashMap::new();
        for i in (1..args.len()).step_by(2) {
            by_shard
                .entry(self.shard_of(&args[i]))
                .or_default()
                .push((args[i].to_vec(), args[i + 1].to_vec()));
        }
        let pairs: Vec<(usize, KvPairs)> = by_shard.into_iter().collect();
        let asks: Vec<(usize, Op)> = pairs
            .iter()
            .map(|(s, p)| (*s, Op::Exists(p.iter().map(|(k, _)| k.clone()).collect())))
            .collect();
        self.push_pending_slot(
            conn_id,
            asks.len() as u32,
            Agg::MSetNx { pairs, existing: 0 },
            is_quit,
        );
        for (shard, op) in asks {
            self.msetnx_send(conn_id, seq, shard, op);
        }
    }

    fn msetnx_send(&mut self, conn_id: u64, seq: u64, shard: usize, op: Op) {
        if shard == self.id {
            self.exec_local(conn_id, seq, op);
        } else {
            self.send_to(shard, Inbound::Request { origin: self.id, conn: conn_id, seq, op });
        }
    }

    /// Every shard has answered: `:0` when a key existed, else the sets.
    pub(crate) fn finalize_msetnx(
        &mut self,
        conn_id: u64,
        seq: u64,
        pairs: Vec<(usize, KvPairs)>,
        existing: i64,
    ) {
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        let Some(slot) = c.pending.get_mut((seq - c.next_emit) as usize) else { return };
        if existing > 0 {
            slot.done = Some(SmallReply::from_slice(b":0\r\n"));
            return drain_front(c);
        }
        slot.remaining = pairs.len() as u32;
        slot.agg = Some(Box::new(Agg::First(Some(SmallReply::from_slice(b":1\r\n")))));
        for (shard, p) in pairs {
            self.msetnx_send(conn_id, seq, shard, Op::MSet(p));
        }
    }
}
