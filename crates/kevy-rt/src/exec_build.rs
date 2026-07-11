//! Multi-target route builders — translate a multi-shard [`crate::Route`] into
//! the `(targets, aggregator)` pair that [`crate::exec`] dispatches and folds.
//!
//! Split out of `exec.rs` so that file stays under the 500-LOC house rule.
//! Everything here is still on the same `impl<C: Commands> Shard<C>`.

use kevy_resp::CmdError;
use crate::Commands;
use crate::Route;
use crate::message::{Agg, GatherKind, KeyShape, KvPairs, MultiOp, Op};
use crate::shard::Shard;
use kevy_resp::{Argv, ArgvView};
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
    // LOC-WAIVER: data-driven route dispatch table — one arm per
    // multi-target Route variant mapping to (targets, Agg).
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
            Route::BgSave => (
                (0..self.nshards).map(|s| (s, Op::BgSave)).collect(),
                Agg::AllOk,
            ),
            Route::RewriteAof => (
                (0..self.nshards).map(|s| (s, Op::RewriteAof)).collect(),
                Agg::AllOk,
            ),
            Route::MSet => self.build_mset_targets(args),
            Route::Gather(op) => match op {
                MultiOp::Mget => self.build_gather(args, GatherKind::Str, op),
                MultiOp::SInter | MultiOp::SUnion | MultiOp::SDiff => {
                    self.build_gather(args, GatherKind::Set, op)
                }
                // ZINTERCARD is a numkeys-form argv — its own parser
                // extracts the keys and the LIMIT cap.
                MultiOp::ZInterCard => self.build_zintercard(args),
            },
            Route::ZAlgebraStore(combine) => self.build_zalgebra_store(args, combine),
            Route::Extension => {
                let argv: Vec<Vec<u8>> = (0..args.len()).map(|i| args[i].to_vec()).collect();
                let targets = (0..self.nshards)
                    .map(|s| (s, Op::Extension { argv: argv.clone() }))
                    .collect();
                (targets, Agg::ExtensionGather { argv, chunks: Vec::new() })
            }
            // REPL.TOKEN: every shard reports its live
            // (generation, next_offset) pair; the origin lays them out
            // as one flat integer array in shard order.
            Route::ReplToken => (
                (0..self.nshards).map(|s| (s, Op::ReplToken)).collect(),
                Agg::ReplTokens { slots: vec![None; self.nshards] },
            ),
            // WAIT / REPL.WAIT are intercepted in `start_command`
            // (deferred barriers with their own starters); reaching
            // here is a routing bug — reply an error, don't crash.
            Route::ReplWait { .. } | Route::ReplBarrier { .. } => {
                gather_error("ERR internal: repl-wait route hit multi builder")
            }
            Route::PrefixStats => {
                let prefix = args.get(1).map(|p| p.to_vec()).unwrap_or_default();
                let targets = (0..self.nshards)
                    .map(|s| (s, Op::PrefixStats(prefix.clone())))
                    .collect();
                (targets, Agg::PrefixStats { keys: 0, expires: 0 })
            }
            // FEED.* is intercepted in `start_command` before the
            // multi path; reaching here is a routing bug — resolve
            // with an error reply rather than crashing the shard.
            Route::FeedRead | Route::FeedTail | Route::FeedShards => {
                gather_error("ERR internal: feed route hit multi builder")
            }
            Route::Keyspace(shape, pat) => {
                // RANDOMKEY needs one key per shard; KEYS/SCAN collect all.
                let limit = matches!(shape, KeyShape::Random).then_some(1);
                self.fanout_keys(pat, limit, shape)
            }
            Route::XReadGather { streams, count, group } => {
                self.build_xread_targets(streams, count, group)
            }
        }
    }

    /// One `Op::XReadOne` per stream, routed to its owning shard, tagged with
    /// the stream's request index so the gather reassembles in order. With a
    /// `group` context the sub-query is the XREADGROUP form (a write — the
    /// owning shard AOF-logs the rewritten single-stream command, which also
    /// replays correctly per shard, unlike the original multi-stream argv).
    fn build_xread_targets(
        &self,
        streams: Vec<(Vec<u8>, Vec<u8>)>,
        count: Option<usize>,
        group: Option<crate::XGroupCtx>,
    ) -> (Vec<(usize, Op)>, Agg) {
        let n = streams.len();
        let count_bytes = count.map(|c| c.to_string().into_bytes());
        let targets = streams
            .into_iter()
            .enumerate()
            .map(|(i, (key, cursor))| {
                let shard = self.shard_of(&key);
                let mut argv = Argv::default();
                match &group {
                    Some(g) => {
                        argv.push(b"XREADGROUP");
                        argv.push(b"GROUP");
                        argv.push(&g.group);
                        argv.push(&g.consumer);
                    }
                    None => argv.push(b"XREAD"),
                }
                if let Some(cb) = &count_bytes {
                    argv.push(b"COUNT");
                    argv.push(cb);
                }
                if group.as_ref().is_some_and(|g| g.noack) {
                    argv.push(b"NOACK");
                }
                argv.push(b"STREAMS");
                argv.push(&key);
                argv.push(&cursor);
                (shard, Op::XReadOne { index: i as u32, argv, write: group.is_some() })
            })
            .collect();
        (targets, Agg::XReadGather { slots: vec![None; n] })
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
                .entry(self.shard_of(&args[i]))
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
                .entry(self.shard_of(k))
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
                limit: 0,
                keys,
                got: HashMap::new(),
            },
        )
    }

    /// `Z*STORE dst numkeys key… [WEIGHTS…] [AGGREGATE …]` and
    /// the set forms `S*STORE dst key…`. Parse errors resolve through
    /// an empty target list + a pre-baked error reply (`start_multi`
    /// folds an ignored `Int(0)` into `Agg::First`).
    fn build_zalgebra_store<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        combine: crate::message::ZCombine,
    ) -> (Vec<(usize, Op)>, Agg) {
        use crate::message::ZCombine;
        let zset_form = matches!(combine, ZCombine::ZInter | ZCombine::ZUnion | ZCombine::ZDiff);
        let parsed = if zset_form {
            parse_zsetstore_args(args, matches!(combine, ZCombine::ZDiff))
        } else {
            parse_setstore_args(args)
        };
        let (dst, keys, weights, aggregate) = match parsed {
            Ok(t) => t,
            Err(msg) => return gather_error(msg.as_wire()),
        };
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for k in &keys {
            by_shard.entry(self.shard_of(k)).or_default().push(k.clone());
        }
        let kind = if zset_form { GatherKind::Scored } else { GatherKind::Set };
        let targets = by_shard
            .into_iter()
            .map(|(s, ks)| (s, Op::Gather(kind, ks)))
            .collect();
        (
            targets,
            Agg::ZStoreGather {
                combine,
                weights,
                aggregate,
                dst,
                keys,
                got: HashMap::new(),
            },
        )
    }

    /// `ZINTERCARD numkeys key… [LIMIT n]`.
    fn build_zintercard<A: ArgvView + ?Sized>(&self, args: &A) -> (Vec<(usize, Op)>, Agg) {
        let (keys, limit) = match parse_zintercard_args(args) {
            Ok(t) => t,
            Err(msg) => return gather_error(msg.as_wire()),
        };
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for k in &keys {
            by_shard.entry(self.shard_of(k)).or_default().push(k.clone());
        }
        let targets = by_shard
            .into_iter()
            .map(|(s, ks)| (s, Op::Gather(GatherKind::Scored, ks)))
            .collect();
        (
            targets,
            Agg::Gather {
                op: MultiOp::ZInterCard,
                limit,
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
                .entry(self.shard_of(key))
                .or_default()
                .push(key.to_vec());
        }
        by_shard
            .into_iter()
            .map(|(s, keys)| (s, mk(keys)))
            .collect()
    }
}


/// Empty-target error resolution: `start_multi` pushes remaining=1 and
/// folds an `Int(0)` the `Agg::First` ignores; materialize then emits
/// the pre-baked error.
fn gather_error(msg: &'static str) -> (Vec<(usize, Op)>, Agg) {
    let mut out = Vec::new();
    kevy_resp::encode_error(&mut out, msg);
    (Vec::new(), Agg::First(Some(crate::message::SmallReply::from_vec(out))))
}

type ZStoreParsed = (Vec<u8>, Vec<Vec<u8>>, Option<Vec<f64>>, kevy_store::ZAggregate);

/// `VERB dst numkeys key… [WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]`
/// (`diff_form` = ZDIFFSTORE: no WEIGHTS/AGGREGATE allowed).
fn parse_zsetstore_args<A: ArgvView + ?Sized>(
    args: &A,
    diff_form: bool,
) -> Result<ZStoreParsed, CmdError> {
    if args.len() < 4 {
        return Err(CmdError::Wire("ERR wrong number of arguments"));
    }
    let dst = args[1].to_vec();
    let numkeys: usize = std::str::from_utf8(&args[2])
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .ok_or("ERR numkeys should be greater than 0")?;
    if args.len() < 3 + numkeys {
        return Err(CmdError::Wire("ERR Number of keys can't be greater than number of args"));
    }
    let keys: Vec<Vec<u8>> = (3..3 + numkeys).map(|i| args[i].to_vec()).collect();
    let (weights, aggregate) = parse_zstore_tail(args, diff_form, numkeys)?;
    Ok((dst, keys, weights, aggregate))
}

/// The optional `[WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]` tail of the
/// zset-store arg forms. Extracted verbatim from
/// [`parse_zsetstore_args`] (single call site, `inline(always)`) purely
/// for the 50-LOC fn rule.
#[inline(always)]
fn parse_zstore_tail<A: ArgvView + ?Sized>(
    args: &A,
    diff_form: bool,
    numkeys: usize,
) -> Result<(Option<Vec<f64>>, kevy_store::ZAggregate), CmdError> {
    let mut weights = None;
    let mut aggregate = kevy_store::ZAggregate::Sum;
    let mut i = 3 + numkeys;
    while i < args.len() {
        let a = &args[i];
        if !diff_form && a.eq_ignore_ascii_case(b"WEIGHTS") {
            if args.len() < i + 1 + numkeys {
                return Err(CmdError::Wire("ERR syntax error"));
            }
            let mut w = Vec::with_capacity(numkeys);
            for j in 0..numkeys {
                let v = std::str::from_utf8(&args[i + 1 + j])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or("ERR weight value is not a float")?;
                w.push(v);
            }
            weights = Some(w);
            i += 1 + numkeys;
        } else if !diff_form && a.eq_ignore_ascii_case(b"AGGREGATE") {
            let m = args.get(i + 1).ok_or("ERR syntax error")?;
            aggregate = if m.eq_ignore_ascii_case(b"SUM") {
                kevy_store::ZAggregate::Sum
            } else if m.eq_ignore_ascii_case(b"MIN") {
                kevy_store::ZAggregate::Min
            } else if m.eq_ignore_ascii_case(b"MAX") {
                kevy_store::ZAggregate::Max
            } else {
                return Err(CmdError::Wire("ERR syntax error"));
            };
            i += 2;
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
    }
    Ok((weights, aggregate))
}

/// `S*STORE dst key [key …]`.
fn parse_setstore_args<A: ArgvView + ?Sized>(args: &A) -> Result<ZStoreParsed, CmdError> {
    if args.len() < 3 {
        return Err(CmdError::Wire("ERR wrong number of arguments"));
    }
    let dst = args[1].to_vec();
    let keys: Vec<Vec<u8>> = (2..args.len()).map(|i| args[i].to_vec()).collect();
    Ok((dst, keys, None, kevy_store::ZAggregate::Sum))
}

/// `ZINTERCARD numkeys key… [LIMIT n]`.
fn parse_zintercard_args<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(Vec<Vec<u8>>, usize), CmdError> {
    if args.len() < 3 {
        return Err(CmdError::Wire("ERR wrong number of arguments"));
    }
    let numkeys: usize = std::str::from_utf8(&args[1])
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .ok_or("ERR numkeys should be greater than 0")?;
    if args.len() < 2 + numkeys {
        return Err(CmdError::Wire("ERR Number of keys can't be greater than number of args"));
    }
    let keys: Vec<Vec<u8>> = (2..2 + numkeys).map(|i| args[i].to_vec()).collect();
    let mut limit = 0usize;
    let mut i = 2 + numkeys;
    while i < args.len() {
        if args[i].eq_ignore_ascii_case(b"LIMIT") {
            limit = args
                .get(i + 1)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|s| s.parse().ok())
                .ok_or("ERR LIMIT can't be negative")?;
            i += 2;
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
    }
    Ok((keys, limit))
}
