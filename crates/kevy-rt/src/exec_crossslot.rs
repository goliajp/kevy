//! v1.56 — cluster-mode CROSSSLOT check helpers for multi-key commands.
//!
//! Lives outside `exec.rs` to keep that file under the 500-LOC house
//! rule. Used only by `Shard::start_command` when `cluster_conn` is
//! true.

use kevy_resp::ArgvView;

use crate::message::{Agg, Part, SmallReply};
use crate::shard::Shard;
use crate::{Commands, Route};

impl<C: Commands> Shard<C> {
    /// v1.56: if the route is a multi-key route checked under cluster
    /// mode AND the conn is cluster AND its keys span slots, push a
    /// `-CROSSSLOT` reply; else fall through to the standard `start_multi`
    /// fan-out path.
    pub(crate) fn start_multi_or_crossslot<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        route: Route,
        is_quit: bool,
        cluster_conn: bool,
    ) {
        if cluster_conn && is_crossslot_checked(&route) && keys_span_slots(&route, args) {
            self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
            self.fold(
                conn_id, seq,
                Part::Reply(SmallReply::from_vec(
                    b"-CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec(),
                )),
            );
            return;
        }
        self.start_multi(conn_id, seq, args, route, is_quit);
    }
}


/// Routes whose keys must all hash to the same CRC16 slot under
/// cluster mode (per the Redis Cluster spec). Other multi-key routes
/// (DEL / EXISTS / SUBSCRIBE / DBSIZE) legally span slots and are NOT
/// checked.
pub(crate) fn is_crossslot_checked(route: &Route) -> bool {
    matches!(
        route,
        Route::Gather(_) | Route::MSet | Route::ZAlgebraStore(_)
    )
}

/// `true` when at least two keys in `args` hash to different CRC16
/// slots. `route` selects how to walk the argv (MSET uses every other
/// arg starting at 1; everything else uses args[1..]). Short-circuits
/// on the first slot disagreement.
pub(crate) fn keys_span_slots<A: ArgvView + ?Sized>(route: &Route, args: &A) -> bool {
    // numkeys-form argv: keys are NOT a uniform args[1..] walk.
    //   ZINTERSTORE/ZUNIONSTORE/ZDIFFSTORE dst numkeys k…  → dst + args[3..3+n]
    //   ZINTERCARD numkeys k…                              → args[2..2+n]
    // (the set-form *STOREs are a plain dst+keys walk, handled below).
    match route {
        Route::ZAlgebraStore(
            crate::ZCombine::ZInter | crate::ZCombine::ZUnion | crate::ZCombine::ZDiff,
        ) => {
            let Some(n) = parse_numkeys(args, 2) else { return false };
            let mut slots = key_slot_at(args, 1).into_iter().collect::<Vec<_>>();
            for i in 3..(3 + n).min(args.len()) {
                if let Some(sl) = key_slot_at(args, i) {
                    slots.push(sl);
                }
            }
            return slots.windows(2).any(|w| w[0] != w[1]);
        }
        Route::Gather(crate::MultiOp::ZInterCard) => {
            let Some(n) = parse_numkeys(args, 1) else { return false };
            let mut slots = Vec::new();
            for i in 2..(2 + n).min(args.len()) {
                if let Some(sl) = key_slot_at(args, i) {
                    slots.push(sl);
                }
            }
            return slots.windows(2).any(|w| w[0] != w[1]);
        }
        _ => {}
    }
    let step = if matches!(route, Route::MSet) { 2 } else { 1 };
    let n = args.len();
    if n < 1 + step + 1 {
        return false;
    }
    let mut i = 1;
    let Some(first) = args.get(i) else { return false };
    let first_slot = kevy_hash::key_hash_slot(first);
    i += step;
    while i < n {
        let Some(k) = args.get(i) else { break };
        if kevy_hash::key_hash_slot(k) != first_slot {
            return true;
        }
        i += step;
    }
    false
}

fn parse_numkeys<A: ArgvView + ?Sized>(args: &A, idx: usize) -> Option<usize> {
    args.get(idx)
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
}

fn key_slot_at<A: ArgvView + ?Sized>(args: &A, idx: usize) -> Option<u16> {
    args.get(idx).map(kevy_hash::key_hash_slot)
}
