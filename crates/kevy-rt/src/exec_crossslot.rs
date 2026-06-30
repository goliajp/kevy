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
        Route::MGet | Route::MSet | Route::SInter | Route::SUnion | Route::SDiff
    )
}

/// `true` when at least two keys in `args` hash to different CRC16
/// slots. `route` selects how to walk the argv (MSET uses every other
/// arg starting at 1; everything else uses args[1..]). Short-circuits
/// on the first slot disagreement.
pub(crate) fn keys_span_slots<A: ArgvView + ?Sized>(route: &Route, args: &A) -> bool {
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
