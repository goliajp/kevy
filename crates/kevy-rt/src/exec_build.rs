//! Multi-target route builders — translate a multi-shard [`crate::Route`] into
//! the `(targets, aggregator)` pair that [`crate::exec`] dispatches and folds.
//!
//! Split out of `exec.rs` so that file stays under the 500-LOC house rule.
//! Everything here is still on the same `impl<C: Commands> Shard<C>`.

use crate::Commands;
use crate::Route;
use crate::message::{Agg, GatherKind, KeyShape, KvPairs, MultiOp, Op};
use crate::reduce::shard_of;
use crate::shard::Shard;
use kevy_resp::ArgvView;
use std::collections::HashMap;

impl<C: Commands> Shard<C> {
    /// Translate a multi-target [`Route`] into a `(targets, aggregator)` pair.
    /// `route` is consumed so `Keys(pat)` / `Scan(pat)` can move the owned
    /// pattern into `fanout_keys` without an extra clone.
    ///
    /// Single-target and pub/sub routes should never reach here — they go
    /// through dedicated paths in `start_command`. If a routing-layer bug
    /// ever sends one through, we emit a WARN and produce an empty target
    /// list so the conn gets a nil/0 reply rather than crashing the
    /// reactor for every other in-flight command on the shard. The
    /// connection sees an observably-wrong reply; nothing else dies.
    pub(crate) fn build_multi_targets<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        route: Route,
    ) -> (Vec<(usize, Op)>, Agg) {
        match route {
            Route::Local | Route::Single(_) => {
                eprintln!(
                    "kevy WARN: build_multi_targets reached single-target route {route:?} \
                     — routing bug; replying nil to the client"
                );
                (Vec::new(), Agg::First(None))
            }
            Route::Subscribe
            | Route::Unsubscribe
            | Route::Psubscribe
            | Route::Punsubscribe
            | Route::Publish
            | Route::Watch
            | Route::Unwatch
            | Route::Hello
            | Route::Rename { .. }
            | Route::Slowlog(_) => {
                eprintln!(
                    "kevy WARN: build_multi_targets reached conn-level route {route:?} \
                     — routing bug; replying nil to the client"
                );
                (Vec::new(), Agg::First(None))
            }
            Route::DelKeys => (self.group_keys(args, Op::Del), Agg::SumInt(0)),
            Route::ExistsKeys => (self.group_keys(args, Op::Exists), Agg::SumInt(0)),
            Route::Dbsize => (
                (0..self.nshards).map(|s| (s, Op::Dbsize)).collect(),
                Agg::SumInt(0),
            ),
            Route::Flush => (
                (0..self.nshards).map(|s| (s, Op::Flush)).collect(),
                Agg::AllOk,
            ),
            Route::Save => (
                (0..self.nshards).map(|s| (s, Op::Save)).collect(),
                Agg::AllOk,
            ),
            Route::RewriteAof => (
                (0..self.nshards).map(|s| (s, Op::RewriteAof)).collect(),
                Agg::AllOk,
            ),
            Route::MSet => self.build_mset_targets(args),
            Route::MGet => self.build_gather(args, GatherKind::Str, MultiOp::Mget),
            Route::SInter => self.build_gather(args, GatherKind::Set, MultiOp::SInter),
            Route::SUnion => self.build_gather(args, GatherKind::Set, MultiOp::SUnion),
            Route::SDiff => self.build_gather(args, GatherKind::Set, MultiOp::SDiff),
            Route::Keys(pat) => self.fanout_keys(pat, None, KeyShape::Keys),
            Route::Scan(pat) => self.fanout_keys(pat, None, KeyShape::Scan),
            Route::RandomKey => self.fanout_keys(None, Some(1), KeyShape::Random),
        }
    }

    /// Group `args[1..]` key/value pairs by each key's shard for MSET.
    fn build_mset_targets<A: ArgvView + ?Sized>(
        &self,
        args: &A,
    ) -> (Vec<(usize, Op)>, Agg) {
        let mut by_shard: HashMap<usize, KvPairs> = HashMap::new();
        let mut i = 1;
        while i + 1 < args.len() {
            by_shard
                .entry(shard_of(&args[i], self.nshards))
                .or_default()
                .push((args[i].to_vec(), args[i + 1].to_vec()));
            i += 2;
        }
        (
            by_shard
                .into_iter()
                .map(|(s, p)| (s, Op::MSet(p)))
                .collect(),
            Agg::AllOk,
        )
    }

    /// Group `args[1..]` keys by shard for a cross-shard gather.
    fn build_gather<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        kind: GatherKind,
        op: MultiOp,
    ) -> (Vec<(usize, Op)>, Agg) {
        let keys: Vec<Vec<u8>> = (1..args.len()).map(|i| args[i].to_vec()).collect();
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for k in &keys {
            by_shard
                .entry(shard_of(k, self.nshards))
                .or_default()
                .push(k.clone());
        }
        let targets = by_shard
            .into_iter()
            .map(|(s, ks)| (s, Op::Gather(kind, ks)))
            .collect();
        (
            targets,
            Agg::Gather {
                op,
                keys,
                got: HashMap::new(),
            },
        )
    }

    /// Fan a key-collection out to every shard (KEYS/SCAN/RANDOMKEY).
    fn fanout_keys(
        &self,
        pat: Option<Vec<u8>>,
        limit: Option<usize>,
        shape: KeyShape,
    ) -> (Vec<(usize, Op)>, Agg) {
        let targets = (0..self.nshards)
            .map(|s| (s, Op::CollectKeys(pat.clone(), limit)))
            .collect();
        (
            targets,
            Agg::Keys {
                shape,
                acc: Vec::new(),
            },
        )
    }

    /// Split `args[1..]` (keys) by owning shard.
    pub(crate) fn group_keys<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        mk: fn(Vec<Vec<u8>>) -> Op,
    ) -> Vec<(usize, Op)> {
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for i in 1..args.len() {
            let key = &args[i];
            by_shard
                .entry(shard_of(key, self.nshards))
                .or_default()
                .push(key.to_vec());
        }
        by_shard
            .into_iter()
            .map(|(s, keys)| (s, mk(keys)))
            .collect()
    }
}
