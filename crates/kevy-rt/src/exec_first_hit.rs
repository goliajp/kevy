//! [`crate::Route::FirstHit`]: `ZMPOP` / `LMPOP`, whose keys may live on
//! different shards. Keys on one shard run there as the command was sent,
//! which is Redis's atomic form; keys apart are tried one at a time, in
//! order, each as the command naming that key alone, so the left-to-right
//! priority holds while the pop is not atomic across them.

use kevy_resp::{Argv, ArgvView, RespVersion};

use crate::message::{Agg, DispatchMeta, Inbound, Op, Part, SmallReply};
use crate::reduce::drain_front;
use crate::shard::Shard;
use crate::{Commands, VerbId};

/// A try writes the key it names, which the single-key form puts at 2.
pub(crate) const FIRST_HIT_META: DispatchMeta = DispatchMeta {
    is_write: true,
    wake_idx: None,
    key_idx: Some(2),
    verb: VerbId::UNKNOWN,
    key_hash: 0,
};

impl<C: Commands> Shard<C> {
    /// Whether every key in `keys` lives on one shard.
    pub(crate) fn one_shard<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        keys: std::ops::Range<usize>,
    ) -> bool {
        let mut shards = keys.map(|i| self.shard_of(&args[i]));
        let first = shards.next();
        shards.all(|s| Some(s) == first)
    }

    /// The keys are spread over shards (see [`Self::one_shard`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_first_hit<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        proto: RespVersion,
        args: &A,
        numkeys: usize,
        is_quit: bool,
        cluster_conn: bool,
    ) {
        self.push_pending_single(conn_id, is_quit);
        if cluster_conn {
            let e = b"-CROSSSLOT Keys in request don't hash to the same slot\r\n";
            return self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(e)));
        }
        let tries: Vec<(usize, Argv)> =
            (2..2 + numkeys).map(|i| (self.shard_of(&args[i]), alone(args, i, numkeys))).collect();
        self.try_next(conn_id, seq, proto, tries, 0);
    }

    /// Arm the slot for try `next` and send it to its key's shard.
    fn try_next(
        &mut self,
        conn_id: u64,
        seq: u64,
        proto: RespVersion,
        mut tries: Vec<(usize, Argv)>,
        next: usize,
    ) {
        let (shard, argv) = (tries[next].0, std::mem::take(&mut tries[next].1));
        if let Some(slot) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|c| c.pending.get_mut((seq - c.next_emit) as usize))
        {
            slot.remaining = 1;
            slot.agg = Some(Box::new(Agg::FirstHit { tries, next: next + 1, got: None }));
        }
        let op = Op::FirstHitTry { argv, proto };
        if shard == self.id {
            self.exec_local(conn_id, seq, op);
        } else {
            self.send_to(shard, Inbound::Request { origin: self.id, conn: conn_id, seq, op });
        }
    }

    /// A try has answered: a null moves on to the next key, anything else
    /// (what was popped, or an error) is the reply.
    pub(crate) fn finalize_first_hit(
        &mut self,
        conn_id: u64,
        seq: u64,
        tries: Vec<(usize, Argv)>,
        next: usize,
        got: Option<SmallReply>,
    ) {
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        let idx = (seq - c.next_emit) as usize;
        let reply = got.unwrap_or_else(|| SmallReply::from_slice(b"*-1\r\n"));
        let null = matches!(reply.bytes(&[]), b"*-1\r\n" | b"_\r\n");
        if null && next < tries.len() {
            let proto = c.pending.get(idx).map_or(RespVersion::V2, |s| s.proto);
            return self.try_next(conn_id, seq, proto, tries, next);
        }
        if let Some(slot) = c.pending.get_mut(idx) {
            slot.done = Some(reply);
        }
        drain_front(c);
    }
}

/// The command naming key `at` alone: `VERB 1 key <tail after the keys>`.
fn alone<A: ArgvView + ?Sized>(args: &A, at: usize, numkeys: usize) -> Argv {
    let mut a = Argv::default();
    a.push(&args[0]);
    a.push(b"1");
    a.push(&args[at]);
    for i in 2 + numkeys..args.len() {
        a.push(&args[i]);
    }
    a
}
