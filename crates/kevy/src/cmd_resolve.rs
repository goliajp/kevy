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
use kevy_rt::{ResolvedCmd, Route, TxnKind, parse_slowlog_sub};

use crate::cmd::{self, scan_pattern, upper_verb};
use crate::cmd_block;

/// One-pass verb resolution for [`crate::KevyCommands`]. Single `match upper`
/// fans out into the per-attribute fields the runtime then consumes.
pub(crate) fn kevy_resolve<A: ArgvView + ?Sized>(args: &A) -> ResolvedCmd {
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

    let txn_kind = match upper {
        b"MULTI" => TxnKind::Multi,
        b"EXEC" => TxnKind::Exec,
        b"DISCARD" => TxnKind::Discard,
        b"WATCH" => TxnKind::Watch,
        _ => TxnKind::Other,
    };

    let is_quit = upper == b"QUIT";
    let is_write = cmd::is_write_verb(upper);
    let route = route_for_verb(upper, args);
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

/// Map an uppercased verb + its argv to the routing decision the
/// runtime uses to pick local-fast-path / single-shard / multi-target
/// / pub/sub / transactional control. Pure data; the cost is one `match
/// upper` plus the small extractor calls (KEYS pattern, SCAN cursor,
/// XREAD STREAMS key, SLOWLOG sub-command).
fn route_for_verb<A: ArgvView + ?Sized>(upper: &[u8], args: &A) -> Route {
    match upper {
        b"HELLO" => Route::Hello,
        b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG" | b"INFO" | b"CLUSTER" | b"DEBUG"
        | b"WAIT" | b"SHUTDOWN" | b"CLIENT" | b"SELECT" | b"BLPOP" | b"BRPOP" => Route::Local,
        b"DBSIZE" => Route::Dbsize,
        b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
        b"SAVE" => Route::Save,
        b"BGSAVE" => Route::BgSave,
        b"BGREWRITEAOF" => Route::RewriteAof,
        b"MSET" if args.len() >= 3 && !args.len().is_multiple_of(2) => Route::MSet,
        b"MGET" if args.len() >= 2 => Route::MGet,
        b"SINTER" if args.len() >= 2 => Route::SInter,
        b"SUNION" if args.len() >= 2 => Route::SUnion,
        b"SDIFF" if args.len() >= 2 => Route::SDiff,
        b"KEYS" if args.len() == 2 => Route::Keys(Some(args[1].to_vec())),
        b"SCAN" if args.len() >= 2 => Route::Scan(scan_pattern(args)),
        b"RANDOMKEY" if args.len() == 1 => Route::RandomKey,
        b"SUBSCRIBE" if args.len() >= 2 => Route::Subscribe,
        b"UNSUBSCRIBE" => Route::Unsubscribe,
        b"PSUBSCRIBE" if args.len() >= 2 => Route::Psubscribe,
        b"PUNSUBSCRIBE" => Route::Punsubscribe,
        b"PUBLISH" if args.len() == 3 => Route::Publish,
        b"WATCH" if args.len() >= 2 => Route::Watch,
        b"UNWATCH" => Route::Unwatch,
        b"RENAME" => Route::Rename { nx: false },
        b"RENAMENX" => Route::Rename { nx: true },
        // (BLPOP / BRPOP fold into the Local-routed verb list above —
        // they park on the conn's own origin shard, from where the
        // cross-shard arbiter fans watch registrations out to each key's
        // owning shard, see kevy_rt::block_xshard. Routing by key would
        // strand the waiter on a shard that doesn't own the connection.)
        // v1.27.1: EVAL/EVALSHA route by KEYS[1] (at argv[3]) when
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
        b"DEL" => {
            if args.len() == 2 {
                Route::Single(1)
            } else {
                Route::DelKeys
            }
        }
        b"EXISTS" => {
            if args.len() == 2 {
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
