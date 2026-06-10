//! BLOCK reactor helpers: block-hint classification, wake-source verbs,
//! and route overrides for the stream verbs whose routing key is not
//! `args[1]`. Lifted out of `cmd.rs` to keep that file under the 500-LOC
//! house rule.
//!
//! These all run on the runtime's per-command hot path (one verb-table
//! lookup each, folded into `KevyCommands::resolve`), so every match
//! arm uses the caller's already-uppercased `upper` buffer — no per-cmd
//! allocation. The classifications are deliberately conservative: anything
//! the dispatcher can't safely park (multi-key BLPOP, malformed BLOCK ms,
//! a missing STREAMS clause) returns `BlockHint::None` so the command's
//! own `dispatch_into` gets to emit the precise error reply, and the
//! dispatcher sees output and skips registration.

use kevy_resp::{Argv, ArgvView};
use kevy_rt::{BlockHint, BlockKind, Route, Store, XGroupCtx};

/// Classify an uppercased verb into its blocking-command hint. The runtime
/// uses this (via [`crate::KevyCommands::resolve`]) to know whether to park
/// the conn when the command's `dispatch_into` produces no reply, and on
/// which key(s). `None` is the zero-cost answer for every non-blocking verb.
///
/// Multi-key forms (`BLPOP k1 k2 … timeout`, `XREAD … STREAMS s1 s2 …`) are
/// supported since v2-7e: the runtime parks the conn on its origin shard and
/// fans watch registrations out to each key's owning shard (see
/// `kevy_rt::block_xshard`). The keys are returned in request order.
pub(crate) fn block_hint_for_verb<A: ArgvView + ?Sized>(
    upper: &[u8],
    args: &A,
) -> BlockHint {
    match upper {
        b"BLPOP" => blpop_hint(BlockKind::Blpop, args),
        b"BRPOP" => blpop_hint(BlockKind::Brpop, args),
        b"XREAD" => xread_block_hint(args),
        b"XREADGROUP" => xreadgroup_block_hint(args),
        _ => BlockHint::None,
    }
}

/// `BLPOP key [key …] timeout` — last arg is the timeout, the rest are
/// watched list keys (≥ 1). Malformed timeout → `None` so `cmd_blpop`
/// emits the precise error and the dispatcher skips registration.
fn blpop_hint<A: ArgvView + ?Sized>(kind: BlockKind, args: &A) -> BlockHint {
    if args.len() < 3 {
        return BlockHint::None;
    }
    let timeout_idx = args.len() - 1;
    let Ok(timeout_str) = std::str::from_utf8(&args[timeout_idx]) else {
        return BlockHint::None;
    };
    let Ok(secs) = timeout_str.parse::<f64>() else {
        return BlockHint::None;
    };
    if !secs.is_finite() || secs < 0.0 {
        return BlockHint::None;
    }
    let timeout_ms = (secs * 1000.0) as u64;
    let keys = (1..timeout_idx).map(|i| args[i].to_vec()).collect();
    BlockHint::Block {
        kind,
        keys,
        timeout_ms,
    }
}

/// XREAD: scan `[COUNT n] [BLOCK ms] STREAMS key …` to discover both
/// the `BLOCK ms` clause and the first stream key (the key the conn
/// will be parked on). Returns `BlockHint::None` when:
/// - no `BLOCK` keyword (non-blocking XREAD — caller dispatches normally),
/// - malformed `BLOCK ms` (timeout not an integer),
/// - missing `STREAMS` keyword,
/// - missing stream key after `STREAMS`.
///
/// In each non-block case, `cmd_xread` itself emits the appropriate
/// reply (success or error), so the dispatcher sees output and skips
/// registration.
fn xread_block_hint<A: ArgvView + ?Sized>(args: &A) -> BlockHint {
    let mut block_ms: Option<u64> = None;
    let mut i = 1usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"COUNT" => i = i.saturating_add(2),
            b"BLOCK" => {
                let Some(ms_arg) = args.get(i + 1) else {
                    return BlockHint::None;
                };
                let Ok(s) = std::str::from_utf8(ms_arg) else {
                    return BlockHint::None;
                };
                let Ok(ms) = s.parse::<u64>() else {
                    return BlockHint::None;
                };
                block_ms = Some(ms);
                i = i.saturating_add(2);
            }
            b"STREAMS" => {
                let Some(bm) = block_ms else {
                    return BlockHint::None;
                };
                let Some(keys) = streams_keys(args, i + 1) else {
                    return BlockHint::None;
                };
                return BlockHint::Block {
                    kind: BlockKind::XReadBlock,
                    keys,
                    timeout_ms: bm,
                };
            }
            _ => return BlockHint::None,
        }
    }
    BlockHint::None
}

/// All STREAMS keys for a `STREAMS k1 … kn id1 … idn` tail starting at
/// `start` (the first key). `None` if the key/id count is unbalanced or
/// empty — the caller treats that as "not parkable".
fn streams_keys<A: ArgvView + ?Sized>(args: &A, start: usize) -> Option<Vec<Vec<u8>>> {
    let rest = args.len().checked_sub(start)?;
    if rest == 0 || !rest.is_multiple_of(2) {
        return None;
    }
    let n = rest / 2;
    Some((start..start + n).map(|i| args[i].to_vec()).collect())
}

/// XREADGROUP: like [`xread_block_hint`] but starts after `GROUP gname
/// consumer` (which precedes any other option), and tolerates the bare
/// `NOACK` flag in the option scan. The first STREAMS key is the parked
/// key when BLOCK is set.
fn xreadgroup_block_hint<A: ArgvView + ?Sized>(args: &A) -> BlockHint {
    if args.len() < 4 || !args[1].eq_ignore_ascii_case(b"GROUP") {
        return BlockHint::None;
    }
    let mut block_ms: Option<u64> = None;
    let mut i = 4usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"COUNT" => i = i.saturating_add(2),
            b"BLOCK" => {
                let Some(ms_arg) = args.get(i + 1) else {
                    return BlockHint::None;
                };
                let Ok(s) = std::str::from_utf8(ms_arg) else {
                    return BlockHint::None;
                };
                let Ok(ms) = s.parse::<u64>() else {
                    return BlockHint::None;
                };
                block_ms = Some(ms);
                i = i.saturating_add(2);
            }
            b"NOACK" => i = i.saturating_add(1),
            b"STREAMS" => {
                let Some(bm) = block_ms else {
                    return BlockHint::None;
                };
                let Some(keys) = streams_keys(args, i + 1) else {
                    return BlockHint::None;
                };
                // XREADGROUP BLOCK only parks for `>`-mode streams; the
                // dispatcher cannot tell that from BlockHint alone, but
                // cmd_xreadgroup leaves `out` untouched only when at
                // least one stream is in `>` mode, so a replay-mode
                // call produces output and the registration is skipped.
                return BlockHint::Block {
                    kind: BlockKind::XReadGroupBlock,
                    keys,
                    timeout_ms: bm,
                };
            }
            _ => return BlockHint::None,
        }
    }
    BlockHint::None
}

/// Routing for `XREAD`. A `BLOCK` clause forces [`Route::Local`] so the
/// conn parks on its own shard and the cross-shard arbiter fans watch
/// registrations out (single- or multi-stream, any shard layout). A
/// non-blocking `XREAD` still routes by the **first STREAMS key** —
/// multi-stream non-blocking reads across shards remain a separate gather
/// (out of v2-7e's blocking scope). Falls back to `Route::Single(1)` on
/// malformed input so `cmd_xread` emits the precise syntax error.
pub(crate) fn xread_route<A: ArgvView + ?Sized>(args: &A) -> Route {
    let mut count: Option<usize> = None;
    let mut i = 1usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"BLOCK" => return Route::Local,
            b"COUNT" => {
                // Malformed COUNT → route single so cmd_xread emits the error.
                match args
                    .get(i + 1)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    Some(c) => count = Some(c),
                    None => return Route::Single(1),
                }
                i = i.saturating_add(2);
            }
            b"STREAMS" => return xread_streams_route(args, i + 1, count, None),
            _ => return Route::Single(1),
        }
    }
    Route::Local
}

/// Decide the route for an `XREAD … STREAMS k1 … kn id1 … idn` tail (start =
/// first key). One stream → route by its key (fast single-shard path);
/// ≥2 streams → a cross-shard gather (fan each stream to its owning shard,
/// merge in request order). Malformed (unbalanced) → `Single(1)` so
/// `cmd_xread` emits the precise error.
fn xread_streams_route<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    count: Option<usize>,
    group: Option<XGroupCtx>,
) -> Route {
    let Some(rest) = args.len().checked_sub(start) else {
        return Route::Single(1);
    };
    if rest == 0 || !rest.is_multiple_of(2) {
        return Route::Single(1);
    }
    let n = rest / 2;
    if n == 1 {
        return Route::Single(start);
    }
    let streams = (0..n)
        .map(|j| (args[start + j].to_vec(), args[start + n + j].to_vec()))
        .collect();
    Route::XReadGather { streams, count, group }
}

/// Routing for `XREADGROUP`: same as [`xread_route`] but starts the scan
/// after `GROUP gname consumer` and tracks the bare `NOACK` flag. One
/// stream routes by its key (single-shard path); ≥2 streams become a
/// cross-shard gather carrying the group context, so streams owned by
/// other shards are read (and their PEL updated) instead of silently
/// dropped — same bug shape the non-blocking multi-stream XREAD had.
pub(crate) fn xreadgroup_route<A: ArgvView + ?Sized>(args: &A) -> Route {
    if args.len() < 4 || !args[1].eq_ignore_ascii_case(b"GROUP") {
        // Too short / not GROUP-form: route single-key so cmd_xreadgroup
        // emits the precise arity error. A bare `XREADGROUP` (len 1) has no
        // args[1] to route by — fall back to Local so the runtime never
        // indexes a missing arg (Route::Single(1) would panic the shard).
        return if args.len() >= 2 { Route::Single(1) } else { Route::Local };
    }
    let mut count: Option<usize> = None;
    let mut noack = false;
    let mut i = 4usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"BLOCK" => return Route::Local,
            b"STREAMS" => {
                if i + 1 >= args.len() {
                    return Route::Local; // cmd_xreadgroup emits the error
                }
                let group = XGroupCtx {
                    group: args[2].to_vec(),
                    consumer: args[3].to_vec(),
                    noack,
                };
                return xread_streams_route(args, i + 1, count, Some(group));
            }
            b"COUNT" => {
                // Malformed COUNT → single-key route so the command body
                // emits the precise syntax error.
                match args
                    .get(i + 1)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    Some(c) => count = Some(c),
                    None => return Route::Single(1),
                }
                i = i.saturating_add(2);
            }
            b"NOACK" => {
                noack = true;
                i = i.saturating_add(1);
            }
            _ => return Route::Single(1),
        }
    }
    Route::Local
}

/// Verbs whose successful write may wake a waiter parked on the key at
/// `args[idx]`. `Some(1)` for the small set that BLOCK readers watch
/// (`LPUSH` / `RPUSH` feed `BLPOP` / `BRPOP`; `XADD` feeds `XREAD BLOCK` /
/// `XREADGROUP BLOCK`); `None` for everything else. The runtime's wake
/// hook is *also* gated on `BlockedClients::is_empty()`, so a `None`-only
/// workload (the steady state) pays one inline `Option` discriminant
/// check on every write.
pub(crate) fn wake_idx_for_verb(upper: &[u8]) -> Option<u8> {
    matches!(upper, b"LPUSH" | b"RPUSH" | b"XADD").then_some(1)
}

/// Materialise the parked argv for an `XREAD BLOCK ... STREAMS k1 ...
/// id1 ...` command. The wake path replays this argv against `cmd_xread`,
/// so any `$` IDs must be resolved here to the stream's current last_id —
/// otherwise the post-wake re-resolve sees the just-written `XADD` entry
/// as the cursor and reports zero new rows, racing the wake into a hang.
///
/// Falls back to `args.to_argv()` (no rewrite) on malformed input — the
/// dispatcher already gated registration on a well-formed `BlockHint`, so
/// the only way this is reached with malformed input is a logic bug
/// elsewhere; returning the original argv keeps the existing failure
/// mode (timeout) instead of crashing the shard.
pub(crate) fn xread_resolve_argv<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
) -> Argv {
    let Some(streams_at) = find_xread_streams_token(args) else {
        return args.to_argv();
    };
    let after = args.len() - (streams_at + 1);
    if after == 0 || !after.is_multiple_of(2) {
        return args.to_argv();
    }
    let n = after / 2;
    let keys_start = streams_at + 1;
    let ids_start = keys_start + n;
    let mut out = Argv::default();
    for j in 0..args.len() {
        let arg = args.get(j).expect("in range");
        let pos = j.wrapping_sub(ids_start);
        if pos < n && arg == b"$" {
            let key = args.get(keys_start + pos).expect("in range");
            let resolved = store
                .xread_dollar_last_id(key)
                .map(|id| id.encode())
                .unwrap_or_else(|_| arg.to_vec());
            out.push(&resolved);
        } else {
            out.push(arg);
        }
    }
    out
}

/// Walk `XREAD`'s option preamble (`COUNT` / `BLOCK`) and return the
/// position of the `STREAMS` token, or `None` if the argv is malformed
/// or `STREAMS` is absent. Unrecognised tokens before `STREAMS` short-
/// circuit to `None` — the caller treats that as "leave argv unchanged".
fn find_xread_streams_token<A: ArgvView + ?Sized>(args: &A) -> Option<usize> {
    let mut i = 1usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"STREAMS" => return Some(i),
            b"COUNT" | b"BLOCK" => i = i.saturating_add(2),
            _ => return None,
        }
    }
    None
}
