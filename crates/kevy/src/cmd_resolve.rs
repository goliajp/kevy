//! [`KevyCommands::resolve`]'s body — extracted from [`crate::lib`] to
//! keep that file under the 500-LOC house rule.
//!
//! The runtime calls `Commands::resolve` once per parsed command and
//! reads back `txn_kind` / `route` / `is_quit` / `is_write` /
//! `block_hint` / `wake_idx` from the returned [`ResolvedCmd`] without
//! re-scanning the verb. Folding every per-attribute scan into one
//! `match upper` is the primary hot-path win — keeping the body in
//! one place makes that contract obvious.

use kevy_resp::ArgvView;
use kevy_rt::{MultiOp, ResolvedCmd, Route, TxnKind, parse_slowlog_sub};

use crate::cmd::{self, scan_args, upper_verb};
use crate::cmd_block;
use crate::state::ReplicationState;

/// One-pass verb resolution for [`crate::KevyCommands`]. Single `match upper`
/// fans out into the per-attribute fields the runtime then consumes.
pub(crate) fn kevy_resolve<A: ArgvView + ?Sized>(
    repl: &ReplicationState,
    args: &A,
) -> ResolvedCmd {
    let Some(name) = args.first() else {
        return ResolvedCmd {
            txn_kind: TxnKind::Other,
            route: Route::Local,
            is_quit: false,
            is_write: false,
            block_hint: kevy_rt::BlockHint::None,
            wake_idx: None,
        };
    };
    let mut buf = [0u8; 32];
    let upper = upper_verb(name, &mut buf);

    // Tier-1 fast path (mirrors `dispatch_with_proto`'s): GET / SET resolve
    // in ONE comparison each instead of walking the txn + route (~40 arms) +
    // is_write + block_hint + wake_idx matches, all of which land in their
    // catch-alls for these two verbs. Field values are byte-identical to
    // what the general path below computes.
    match upper {
        b"GET" | b"SET" => {
            return ResolvedCmd {
                txn_kind: TxnKind::Other,
                route: if args.len() >= 2 { Route::Single(1) } else { Route::Local },
                is_quit: false,
                is_write: upper == b"SET",
                block_hint: kevy_rt::BlockHint::None,
                wake_idx: None,
            };
        }
        _ => {}
    }

    resolve_general(repl, upper, args)
}

/// The general (non-GET/SET) resolution tail: one lookup per
/// [`ResolvedCmd`] field. Single call site in [`kevy_resolve`];
/// `inline(always)` keeps the split codegen-identical to the
/// pre-split fused body (this is still the per-op path for every
/// verb outside the tier-1 pair).
#[inline(always)]
fn resolve_general<A: ArgvView + ?Sized>(
    repl: &ReplicationState,
    upper: &[u8],
    args: &A,
) -> ResolvedCmd {
    let txn_kind = match upper {
        b"MULTI" => TxnKind::Multi,
        b"EXEC" => TxnKind::Exec,
        b"DISCARD" => TxnKind::Discard,
        b"WATCH" => TxnKind::Watch,
        _ => TxnKind::Other,
    };

    let is_quit = upper == b"QUIT";
    let is_write = cmd::is_write_verb(upper);
    let route = route_for_verb(repl, upper, args);
    let block_hint = cmd_block::block_hint_for_verb(upper, args);
    let wake_idx = cmd_block::wake_idx_for_verb(upper);

    ResolvedCmd {
        txn_kind,
        route,
        is_quit,
        is_write,
        block_hint,
        wake_idx,
    }
}

/// [`crate::KevyCommands::route`]'s body — the same verb table
/// [`kevy_resolve`] consults, entered from a raw argv. The single
/// routing table lives in [`route_for_verb`]; a parity test over the
/// whole verb registry holds `route()` == `resolve().route`.
pub(crate) fn route<A: ArgvView + ?Sized>(repl: &ReplicationState, args: &A) -> Route {
    let Some(name) = args.first() else {
        return Route::Local;
    };
    let mut buf = [0u8; 32];
    route_for_verb(repl, upper_verb(name, &mut buf), args)
}

/// Map an uppercased verb + its argv to the routing decision the
/// runtime uses to pick local-fast-path / single-shard / multi-target
/// / pub/sub / transactional control. Pure data; the cost is one `match
/// upper` plus the small extractor calls (KEYS pattern, SCAN cursor,
/// XREAD STREAMS key, SLOWLOG sub-command).
// LOC-WAIVER: data-driven verb → Route match table — one arm per verb.
fn route_for_verb<A: ArgvView + ?Sized>(
    repl: &ReplicationState,
    upper: &[u8],
    args: &A,
) -> Route {
    match upper {
        b"HELLO" => Route::Hello,
        // CLIENT: LIST / KILL fan out (the conn tables are per-shard);
        // the rest answers locally or via the reactor intercept
        // (SETNAME / GETNAME / ID / INFO).
        b"CLIENT" => client_route(args),
        b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG" | b"INFO" | b"CLUSTER" | b"DEBUG"
        | b"SHUTDOWN" | b"SELECT" | b"BLPOP" | b"BRPOP" | b"BZPOPMIN" | b"BRPOPLPUSH"
        // Replication admin: answered from the conn's own shard —
        // args[1] is a host (REPLICAOF/SLAVEOF) or absent (ROLE),
        // never a key to route by.
        | b"ROLE" | b"REPLICAOF" | b"SLAVEOF" => {
            Route::Local
        }
        // Replication barriers. Well-formed happy paths
        // route to the runtime's deferred waiters; every immediate
        // answer (arity / role / gen mismatch) falls back to Local and
        // the cmd_repl dispatch handlers emit the precise reply.
        b"WAIT" => crate::cmd_repl::wait_route(repl, args),
        b"REPL.TOKEN" => crate::cmd_repl::token_route(repl, args),
        b"REPL.WAIT" => crate::cmd_repl::repl_wait_route(repl, args),
        b"DBSIZE" => Route::Dbsize,
        b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
        b"SAVE" => Route::Save,
        b"BGSAVE" => Route::BgSave,
        b"BGREWRITEAOF" => Route::RewriteAof,
        // MEMORY USAGE answers about a KEY, so it has to run on that key's
        // shard. Without this arm it fell through to the default
        // `Route::Single(1)` and was routed by hashing args[1] — the
        // subcommand token — so `MEMORY USAGE k` ran on whichever shard owns
        // the literal string "USAGE" and returned nil for every key that
        // lives elsewhere. The other subcommands answer from instance-wide
        // state and can run anywhere.
        b"MEMORY" if args.len() >= 3 && args[1].eq_ignore_ascii_case(b"USAGE") => Route::Single(2),
        b"MEMORY" => Route::Local,
        b"MSET" if args.len() >= 3 && !args.len().is_multiple_of(2) => Route::MSet,
        b"MGET" if args.len() >= 2 => Route::Gather(MultiOp::Mget),
        b"SINTER" if args.len() >= 2 => Route::Gather(MultiOp::SInter),
        b"SUNION" if args.len() >= 2 => Route::Gather(MultiOp::SUnion),
        b"SDIFF" if args.len() >= 2 => Route::Gather(MultiOp::SDiff),
        b"KEYS" if args.len() == 2 => Route::Keys(Some(args[1].to_vec())),
        b"SCAN" if args.len() >= 2 => Route::Scan(scan_args(args)),
        b"RANDOMKEY" if args.len() == 1 => Route::RandomKey,
        b"SUBSCRIBE" if args.len() >= 2 => Route::Subscribe,
        b"UNSUBSCRIBE" => Route::Unsubscribe,
        b"PSUBSCRIBE" if args.len() >= 2 => Route::Psubscribe,
        b"PUNSUBSCRIBE" => Route::Punsubscribe,
        b"PUBLISH" if args.len() == 3 => Route::Publish,
        b"WATCH" if args.len() >= 2 => Route::Watch,
        b"UNWATCH" => Route::Unwatch,
        b"ZINTERSTORE" if args.len() >= 4 => Route::ZAlgebraStore(kevy_rt::ZCombine::ZInter),
        b"ZUNIONSTORE" if args.len() >= 4 => Route::ZAlgebraStore(kevy_rt::ZCombine::ZUnion),
        b"ZDIFFSTORE" if args.len() >= 4 => Route::ZAlgebraStore(kevy_rt::ZCombine::ZDiff),
        b"SINTERSTORE" if args.len() >= 3 => Route::ZAlgebraStore(kevy_rt::ZCombine::SInter),
        b"SUNIONSTORE" if args.len() >= 3 => Route::ZAlgebraStore(kevy_rt::ZCombine::SUnion),
        b"SDIFFSTORE" if args.len() >= 3 => Route::ZAlgebraStore(kevy_rt::ZCombine::SDiff),
        b"ZINTERCARD" if args.len() >= 3 => Route::Gather(MultiOp::ZInterCard),
        // Geo *STORE: the source and the destination are different keys and
        // neither family puts them where the catch-all below assumes —
        // GEOSEARCHSTORE has dst at argv[1], GEORADIUS[BYMEMBER] has src
        // there with dst buried in the option tail. Route by both (search on
        // the source's shard, write on the destination's); the query-only
        // forms fall through to the single-key route.
        b"GEOSEARCHSTORE" | b"GEORADIUS" | b"GEORADIUSBYMEMBER" => {
            crate::dispatch_geo::geo_store_route(upper, args)
                .unwrap_or(if args.len() >= 2 { Route::Single(1) } else { Route::Local })
        }
        b"IDX.QUERY" if args.len() >= 4 => Route::Extension,
        b"IDX.EXPLAIN" if args.len() >= 2 => Route::Extension,
        b"IDX.REBUILD" if args.len() == 2 => Route::Extension,
        b"IDX.COUNT" if args.len() >= 4 => Route::Extension,
        b"IDX.VERIFY" if args.len() == 2 => Route::Extension,
        b"IDX.LIST" if args.len() == 1 => Route::Extension,
        b"VIEW.QUERY" if args.len() >= 2 => Route::Extension,
        b"VIEW.LIST" if args.len() == 1 => Route::Extension,
        b"VIEW.VERIFY" if args.len() == 2 => Route::Extension,
        b"VIEW.REBUILD" if args.len() == 2 => Route::Extension,
        b"VIEW.EXPLAIN" if args.len() == 2 => Route::Extension,
        // TABLE.DECLARE / TABLE.DROP are Local catalog mutations —
        // they fall through to the default arm like IDX.CREATE.
        b"TABLE.LIST" if args.len() == 1 => Route::Extension,
        b"TABLE.VERIFY" if args.len() == 2 => Route::Extension,
        b"PREFIX.STATS" if args.len() == 2 => Route::PrefixStats,
        b"PREFIX.DIGEST" if args.len() == 2 => Route::Extension,
        b"FEED.READ" if args.len() >= 4 => Route::FeedRead,
        b"FEED.TAIL" if args.len() == 2 => Route::FeedTail,
        b"FEED.SHARDS" if args.len() == 1 => Route::FeedShards,
        // RPOPLPUSH / LMOVE move an element BETWEEN two keys, and
        // the two keys can live on different shards. Without these arms they
        // fell through to `Route::Single(1)` — hash args[1], the SOURCE — and
        // the destination push ran on the source's shard, writing the element
        // into a keyspace no reader would ever look in. The command still
        // returned the moved value. 11 of 12 moves lost the element on an
        // 8-shard server. See `kevy_rt::exec_listmove`.
        //
        // BRPOPLPUSH is NOT here: it is a blocking verb, so it stays
        // `Route::Local` and is served through the park/wake path, which has
        // its own destination-routing fix (see `cmd_block`).
        b"RPOPLPUSH" if args.len() == 3 => Route::ListMove { from_left: false, to_left: true },
        b"LMOVE" if args.len() == 5 => {
            let from_left = args[3].eq_ignore_ascii_case(b"LEFT");
            let to_left = args[4].eq_ignore_ascii_case(b"LEFT");
            Route::ListMove { from_left, to_left }
        }
        // COPY names two keys, so it cannot ride the catch-all: see
        // `kevy_rt::exec_copy` for what routing it by args[1] would do
        // to the destination.
        b"COPY" => Route::Copy,
        b"RENAME" => Route::Rename { nx: false },
        b"RENAMENX" => Route::Rename { nx: true },
        // (BLPOP / BRPOP fold into the Local-routed verb list above —
        // they park on the conn's own origin shard, from where the
        // cross-shard arbiter fans watch registrations out to each key's
        // owning shard, see kevy_rt::block_xshard. Routing by key would
        // strand the waiter on a shard that doesn't own the connection.)
        // EVAL/EVALSHA route by KEYS[1] (at argv[3]) when
        // numkeys ≥ 1, so a multi-shard server lands the script on
        // the shard that owns the keys it'll touch. With numkeys=0
        // the script doesn't touch any specific shard's keyspace, so
        // we let it run on the connection's own shard.
        // SCRIPT subcommands all hit a process-global cache
        // (see `crate::cmd_lua`), so Route::Local is fine for them.
        b"EVAL" | b"EVALSHA" | b"EVAL_RO" | b"EVALSHA_RO" => {
            if args.len() >= 4 {
                let nk = std::str::from_utf8(&args[2])
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                if nk >= 1 && (args.len() as i64) >= 3 + nk {
                    Route::Single(3)
                } else {
                    Route::Local
                }
            } else {
                Route::Local
            }
        }
        b"SCRIPT" => Route::Local,
        b"XREAD" => cmd_block::xread_route(args),
        b"XREADGROUP" => cmd_block::xreadgroup_route(args),
        // XGROUP / XINFO put the stream key at args[2] (after the
        // subcommand), not args[1] — route by the real key so a
        // multi-shard server lands on the shard that owns the stream.
        // Keyless forms (HELP) fall back to Local.
        b"XGROUP" | b"XINFO" => {
            if args.len() >= 3 {
                Route::Single(2)
            } else {
                Route::Local
            }
        }
        b"SLOWLOG" => Route::Slowlog(parse_slowlog_sub(args)),
        b"DEL" | b"UNLINK" => {
            // A one-argument call names no key at all. Routing it to the
            // multi-key path made it an EMPTY delete answering `:0`, where
            // Redis — and this engine's own dispatch arm — say wrong number
            // of arguments. Local is where that guard already lives, and is
            // what the default arm below does with a short call.
            if args.len() < 2 {
                Route::Local
            } else if args.len() == 2 {
                Route::Single(1)
            } else {
                Route::DelKeys
            }
        }
        b"EXISTS" | b"TOUCH" => {
            // Same as DEL/UNLINK above: no key named, so not a fan-out.
            // TOUCH rides with EXISTS because in this engine it is
            // EXISTS — `Route::ExistsKeys` emits `Op::Exists` and sums,
            // which is TOUCH's whole contract here.
            if args.len() < 2 {
                Route::Local
            } else if args.len() == 2 {
                Route::Single(1)
            } else {
                Route::ExistsKeys
            }
        }
        _ => {
            if args.len() >= 2 {
                Route::Single(1)
            } else {
                Route::Local
            }
        }
    }
}

/// CLIENT subcommand routing. `LIST` (bare form) and a well-formed
/// `KILL` fan out to every shard — the conn tables are per-shard.
/// Everything else stays local: SETNAME / GETNAME / ID / INFO are
/// intercepted at the reactor, and malformed KILL / filtered LIST
/// shapes fall through to the dispatch handler's error replies.
fn client_route<A: ArgvView + ?Sized>(args: &A) -> Route {
    let Some(sub) = args.get(1) else { return Route::Local };
    match sub.to_ascii_uppercase().as_slice() {
        b"LIST" if args.len() == 2 => Route::ClientList,
        b"KILL" if kevy_rt::ClientKillFilter::parse(args).is_some() => Route::ClientKill,
        _ => Route::Local,
    }
}
