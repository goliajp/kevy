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

use kevy_resp::ArgvView;
use kevy_rt::{BlockHint, BlockKind, Route};

/// Classify an uppercased verb into its blocking-command hint. The runtime
/// uses this (via [`crate::KevyCommands::resolve`]) to know whether to park
/// the conn on a key when the command's `dispatch_into` produces no reply.
/// `None` is the zero-cost answer for every non-blocking verb.
///
/// Multi-key forms (`BLPOP k1 k2 ... timeout`) deliberately return
/// `BlockHint::None`: a sharded build cannot atomically wait on keys that
/// may live on different shards, and `cmd_blpop` emits an explicit error
/// in that branch (so the dispatcher does see a reply written and does not
/// register a waiter). A future v2-7e sprint can lift the same-shard
/// subset.
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

fn blpop_hint<A: ArgvView + ?Sized>(kind: BlockKind, args: &A) -> BlockHint {
    if args.len() != 3 {
        return BlockHint::None;
    }
    let Ok(timeout_str) = std::str::from_utf8(&args[2]) else {
        return BlockHint::None;
    };
    let Ok(secs) = timeout_str.parse::<f64>() else {
        return BlockHint::None;
    };
    if !secs.is_finite() || secs < 0.0 {
        return BlockHint::None;
    }
    let timeout_ms = (secs * 1000.0) as u64;
    BlockHint::Block {
        kind,
        key: args[1].to_vec(),
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
                let Some(key) = args.get(i + 1) else {
                    return BlockHint::None;
                };
                return BlockHint::Block {
                    kind: BlockKind::XReadBlock,
                    key: key.to_vec(),
                    timeout_ms: bm,
                };
            }
            _ => return BlockHint::None,
        }
    }
    BlockHint::None
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
                let Some(key) = args.get(i + 1) else {
                    return BlockHint::None;
                };
                // XREADGROUP BLOCK only parks for `>`-mode streams; the
                // dispatcher cannot tell that from BlockHint alone, but
                // cmd_xreadgroup leaves `out` untouched only when at
                // least one stream is in `>` mode, so a replay-mode
                // call produces output and the registration is skipped.
                return BlockHint::Block {
                    kind: BlockKind::XReadGroupBlock,
                    key: key.to_vec(),
                    timeout_ms: bm,
                };
            }
            _ => return BlockHint::None,
        }
    }
    BlockHint::None
}

/// Routing for `XREAD`: the routing key is the **first STREAMS key**, not
/// `args[1]` (which is typically `COUNT` / `BLOCK` / `STREAMS` itself).
/// Falls back to `Route::Single(1)` on malformed input so `cmd_xread`
/// gets to emit the precise syntax error — and to `Route::Local` on a
/// keyless XREAD so the local shard returns the empty-reply error
/// without a misleading cross-shard hop.
pub(crate) fn xread_route<A: ArgvView + ?Sized>(args: &A) -> Route {
    let mut i = 1usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"STREAMS" => {
                let key_idx = i + 1;
                return if key_idx < args.len() {
                    Route::Single(key_idx)
                } else {
                    Route::Local
                };
            }
            b"COUNT" | b"BLOCK" => i = i.saturating_add(2),
            _ => return Route::Single(1),
        }
    }
    Route::Local
}

/// Routing for `XREADGROUP`: same as [`xread_route`] but starts the scan
/// after `GROUP gname consumer` and skips the bare `NOACK` flag. Looks
/// up the first STREAMS key, the routing target.
pub(crate) fn xreadgroup_route<A: ArgvView + ?Sized>(args: &A) -> Route {
    if args.len() < 4 || !args[1].eq_ignore_ascii_case(b"GROUP") {
        return Route::Single(1);
    }
    let mut i = 4usize;
    while i < args.len() {
        let upper = args[i].to_ascii_uppercase();
        match upper.as_slice() {
            b"STREAMS" => {
                let key_idx = i + 1;
                return if key_idx < args.len() {
                    Route::Single(key_idx)
                } else {
                    Route::Local
                };
            }
            b"COUNT" | b"BLOCK" => i = i.saturating_add(2),
            b"NOACK" => i = i.saturating_add(1),
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
