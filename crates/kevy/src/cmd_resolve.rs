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
        | b"WAIT" | b"SHUTDOWN" | b"CLIENT" | b"SELECT" => Route::Local,
        b"DBSIZE" => Route::Dbsize,
        b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
        b"SAVE" | b"BGSAVE" => Route::Save,
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
        b"XREAD" => cmd_block::xread_route(args),
        b"XREADGROUP" => cmd_block::xreadgroup_route(args),
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
