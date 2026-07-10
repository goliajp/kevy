//! v3.16 D1+D2 — the consistency-ladder verbs:
//!
//! - `WAIT numreplicas timeout` (D1, Redis-compatible): block until
//!   every shard's `master_repl_offset` is acked by ≥ numreplicas
//!   replicas (all-shard barrier — a kevy write may land on any
//!   shard), reply the MIN achieved count. `timeout 0` is Redis's
//!   "wait forever" — kevy hard-caps it at 60 s (documented).
//! - `REPL.TOKEN` (D2, extension): mint a read-your-writes token —
//!   on a primary, every shard's live `(feed generation, next_offset)`
//!   pair; on a replica, its per-runner `(upstream generation,
//!   applied offset)` view.
//! - `REPL.WAIT g0 off0 [g1 off1 …] [TIMEOUT ms]` (D2, extension):
//!   on a replica, block until every shard has APPLIED the token
//!   (reply `+OK` — a subsequent read is read-your-writes); on
//!   timeout or generation mismatch reply
//!   `-MISDIRECTED writer is <primary>` so the client falls back to
//!   the primary. On a primary the token is trivially satisfied
//!   (`+OK` — you are talking to the writer).
//!
//! Routing split: the happy paths route to the runtime's deferred
//! barriers (`Route::ReplWait` / `Route::ReplToken` /
//! `Route::ReplBarrier` — see `kevy_rt::exec_replwait`); every
//! immediate answer (arity errors, wrong role, gen mismatch) falls
//! back to `Route::Local` and the dispatch handlers here emit the
//! precise reply — the same convention every other verb uses for
//! malformed input.

use kevy_resp::{ArgvView, encode_array_len, encode_error, encode_integer, encode_simple_string};
use kevy_rt::Route;

use crate::ops::wrong_args;
use crate::state::ReplicationState;

/// Default `REPL.WAIT` deadline when no `TIMEOUT ms` clause is given.
const REPL_WAIT_DEFAULT_TIMEOUT_MS: u64 = 1_000;

// ───────────────────────── routing ─────────────────────────

/// `WAIT` route: well-formed on a primary/standalone → the runtime's
/// all-shard ack barrier; anything else answers locally (replica →
/// error, malformed → error, both emitted by [`cmd_wait`]).
pub(crate) fn wait_route<A: ArgvView + ?Sized>(repl: &ReplicationState, args: &A) -> Route {
    if repl.is_replica() {
        return Route::Local;
    }
    match parse_wait_args(args) {
        Some((numreplicas, timeout_ms)) => Route::ReplWait { numreplicas, timeout_ms },
        None => Route::Local,
    }
}

/// `REPL.TOKEN` route: a primary/standalone fans out for live
/// per-shard pairs; a replica (or malformed arity) answers locally
/// from the runner registries.
pub(crate) fn token_route<A: ArgvView + ?Sized>(repl: &ReplicationState, args: &A) -> Route {
    if args.len() == 1 && !repl.is_replica() {
        return Route::ReplToken;
    }
    Route::Local
}

/// `REPL.WAIT` route: a replica whose known upstream generations match
/// the token routes to the runtime's applied barrier; every immediate
/// answer (primary +OK, gen mismatch, arity) is `Local` →
/// [`cmd_repl_wait`].
pub(crate) fn repl_wait_route<A: ArgvView + ?Sized>(repl: &ReplicationState, args: &A) -> Route {
    if !repl.is_replica() {
        return Route::Local;
    }
    let Some(tok) = parse_repl_wait(args) else {
        return Route::Local;
    };
    if !gens_match(repl, &tok) {
        return Route::Local; // dispatch emits -MISDIRECTED / arity error
    }
    Route::ReplBarrier {
        offsets: tok.pairs.iter().map(|(_, off)| *off).collect(),
        timeout_ms: tok.timeout_ms,
        miss: misdirected_reply(repl),
    }
}

// ───────────────────────── dispatch (Local fallbacks) ─────────────────────────

/// `WAIT` reached dispatch: replica → the Redis error; malformed →
/// parse errors; otherwise this is a runtime-less context (embedded
/// direct dispatch — no shards, no replicas) → `:0`.
pub(crate) fn cmd_wait<A: ArgvView + ?Sized>(
    repl: &ReplicationState,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return wrong_args(out, "wait");
    }
    if repl.is_replica() {
        return encode_error(out, "ERR WAIT cannot be used with replica instances");
    }
    if parse_wait_args(args).is_none() {
        return encode_error(out, "ERR value is not an integer or out of range");
    }
    // Direct-dispatch context (kevy::dispatch without the runtime):
    // no shards exist to barrier on and no replica has ever acked.
    encode_integer(out, 0);
}

/// `REPL.TOKEN` reached dispatch: a replica answers its per-runner
/// `(upstream generation, applied offset)` view; a primary here means
/// a runtime-less context (the runtime path fans out per shard).
pub(crate) fn cmd_repl_token<A: ArgvView + ?Sized>(
    repl: &ReplicationState,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() != 1 {
        return wrong_args(out, "repl.token");
    }
    if !repl.is_replica() {
        return encode_error(out, "ERR REPL.TOKEN needs the kevy server runtime on a primary");
    }
    let gens = repl.upstream_gens();
    let offs = repl.applied_runner_offsets();
    encode_array_len(out, (gens.len() * 2) as i64);
    for (g, off) in gens.iter().zip(offs.iter()) {
        encode_integer(out, *g as i64);
        encode_integer(out, *off as i64);
    }
}

/// `REPL.WAIT` reached dispatch: the immediate answers. Primary /
/// standalone → `+OK` (you are already talking to the writer); a
/// replica here means the token failed the gen gate (or arity) —
/// `-MISDIRECTED` / a self-explaining error.
pub(crate) fn cmd_repl_wait<A: ArgvView + ?Sized>(
    repl: &ReplicationState,
    args: &A,
    out: &mut Vec<u8>,
) {
    let Some(tok) = parse_repl_wait(args) else {
        return encode_error(
            out,
            "ERR REPL.WAIT g0 off0 [g1 off1 ...] [TIMEOUT ms] — pass a token from REPL.TOKEN",
        );
    };
    if !repl.is_replica() {
        // The writer itself: everything it wrote is trivially visible.
        return encode_simple_string(out, "OK");
    }
    let gens = repl.upstream_gens();
    if gens.len() != tok.pairs.len() {
        return encode_error(
            out,
            &format!(
                "ERR REPL.WAIT token has {} (gen, offset) pair(s) but this replica follows {} \
                 stream(s); take the token from this server's primary with REPL.TOKEN",
                tok.pairs.len(),
                gens.len(),
            ),
        );
    }
    // Generation mismatch (or a not-yet-learned generation): the
    // token's offset space is not the stream this replica follows.
    out.extend_from_slice(&misdirected_reply(repl));
}

// ───────────────────────── parsing ─────────────────────────

/// Parse `WAIT numreplicas timeout` → `(numreplicas, timeout_ms)`.
/// Rejects non-integers and negatives (Redis: "value is not an
/// integer or out of range").
pub(crate) fn parse_wait_args<A: ArgvView + ?Sized>(args: &A) -> Option<(u32, u64)> {
    if args.len() != 3 {
        return None;
    }
    let need: u32 = parse_u64(&args[1])?.try_into().ok()?;
    let timeout_ms = parse_u64(&args[2])?;
    Some((need, timeout_ms))
}

/// A parsed `REPL.WAIT` token: per-shard `(generation, offset)` pairs
/// + the deadline.
pub(crate) struct ReplWaitToken {
    pub(crate) pairs: Vec<(u64, u64)>,
    pub(crate) timeout_ms: u64,
}

/// Parse `REPL.WAIT g0 off0 [g1 off1 …] [TIMEOUT ms]`. `None` on any
/// malformed shape (odd pair count, non-integers, missing token).
pub(crate) fn parse_repl_wait<A: ArgvView + ?Sized>(args: &A) -> Option<ReplWaitToken> {
    let mut end = args.len();
    let mut timeout_ms = REPL_WAIT_DEFAULT_TIMEOUT_MS;
    if end >= 3 && args[end - 2].eq_ignore_ascii_case(b"TIMEOUT") {
        timeout_ms = parse_u64(&args[end - 1])?;
        end -= 2;
    }
    let n = end.checked_sub(1)?; // token ints after the verb
    if n == 0 || !n.is_multiple_of(2) {
        return None;
    }
    let mut pairs = Vec::with_capacity(n / 2);
    let mut i = 1;
    while i < end {
        let g = parse_u64(&args[i])?;
        let off = parse_u64(&args[i + 1])?;
        pairs.push((g, off));
        i += 2;
    }
    Some(ReplWaitToken { pairs, timeout_ms })
}

/// Do the token's generations match this replica's last-seen upstream
/// generations, slot for slot? A never-seen generation (0) fails —
/// the conservative answer is "go read the primary".
fn gens_match(repl: &ReplicationState, tok: &ReplWaitToken) -> bool {
    let gens = repl.upstream_gens();
    gens.len() == tok.pairs.len()
        && tok
            .pairs
            .iter()
            .zip(gens.iter())
            .all(|((g, _), known)| *g == *known && *g != 0)
}

/// The `-MISDIRECTED writer is <addr>` reply. The address is the live
/// upstream (the `REPLICAOF` target — kevy's upstream addressing is
/// the replication port base); "unknown" when no upstream is
/// installed.
fn misdirected_reply(repl: &ReplicationState) -> Vec<u8> {
    match repl.current_upstream() {
        Some((host, port)) => format!("-MISDIRECTED writer is {host}:{port}\r\n").into_bytes(),
        None => b"-MISDIRECTED writer is unknown\r\n".to_vec(),
    }
}

/// Strict non-negative decimal parse off raw argv bytes.
fn parse_u64(b: &[u8]) -> Option<u64> {
    std::str::from_utf8(b).ok()?.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn argv(parts: &[&str]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p.as_bytes());
        }
        a
    }

    #[test]
    fn parse_wait_args_accepts_valid_and_rejects_garbage() {
        assert_eq!(parse_wait_args(&argv(&["WAIT", "1", "500"])), Some((1, 500)));
        assert_eq!(parse_wait_args(&argv(&["WAIT", "0", "0"])), Some((0, 0)));
        assert_eq!(parse_wait_args(&argv(&["WAIT", "-1", "500"])), None);
        assert_eq!(parse_wait_args(&argv(&["WAIT", "1", "-5"])), None);
        assert_eq!(parse_wait_args(&argv(&["WAIT", "x", "500"])), None);
        assert_eq!(parse_wait_args(&argv(&["WAIT", "1"])), None);
    }

    #[test]
    fn parse_repl_wait_pairs_and_default_timeout() {
        let t = parse_repl_wait(&argv(&["REPL.WAIT", "1", "42"])).unwrap();
        assert_eq!(t.pairs, vec![(1, 42)]);
        assert_eq!(t.timeout_ms, REPL_WAIT_DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn parse_repl_wait_multi_pair_with_timeout_clause() {
        let t = parse_repl_wait(&argv(&[
            "REPL.WAIT", "1", "10", "1", "20", "TIMEOUT", "250",
        ]))
        .unwrap();
        assert_eq!(t.pairs, vec![(1, 10), (1, 20)]);
        assert_eq!(t.timeout_ms, 250);
    }

    #[test]
    fn parse_repl_wait_rejects_malformed() {
        // No token at all.
        assert!(parse_repl_wait(&argv(&["REPL.WAIT"])).is_none());
        // Odd int count (not pairs).
        assert!(parse_repl_wait(&argv(&["REPL.WAIT", "1", "10", "2"])).is_none());
        // Non-integer field.
        assert!(parse_repl_wait(&argv(&["REPL.WAIT", "1", "x"])).is_none());
        // TIMEOUT without a value leaves an odd token → rejected.
        assert!(parse_repl_wait(&argv(&["REPL.WAIT", "1", "10", "TIMEOUT"])).is_none());
        // Bad TIMEOUT value.
        assert!(parse_repl_wait(&argv(&["REPL.WAIT", "1", "10", "TIMEOUT", "x"])).is_none());
    }

    fn primary() -> ReplicationState {
        ReplicationState::new(1, false)
    }

    #[test]
    fn wait_route_shapes() {
        let repl = primary();
        assert!(matches!(
            wait_route(&repl, &argv(&["WAIT", "1", "500"])),
            Route::ReplWait { numreplicas: 1, timeout_ms: 500 }
        ));
        // Malformed → Local (dispatch emits the precise error).
        assert!(matches!(wait_route(&repl, &argv(&["WAIT", "x", "500"])), Route::Local));
        assert!(matches!(wait_route(&repl, &argv(&["WAIT"])), Route::Local));
    }

    #[test]
    fn repl_token_route_is_fanout_on_primary_and_local_on_arity_error() {
        let repl = primary();
        assert!(matches!(token_route(&repl, &argv(&["REPL.TOKEN"])), Route::ReplToken));
        assert!(matches!(token_route(&repl, &argv(&["REPL.TOKEN", "x"])), Route::Local));
    }

    #[test]
    fn repl_wait_on_primary_replies_ok_immediately() {
        let repl = primary();
        assert!(matches!(
            repl_wait_route(&repl, &argv(&["REPL.WAIT", "1", "10"])),
            Route::Local
        ));
        let mut out = Vec::new();
        cmd_repl_wait(&repl, &argv(&["REPL.WAIT", "1", "10"]), &mut out);
        assert_eq!(out, b"+OK\r\n");
    }

    #[test]
    fn repl_wait_malformed_token_errors() {
        let mut out = Vec::new();
        cmd_repl_wait(&primary(), &argv(&["REPL.WAIT", "nope"]), &mut out);
        assert!(out.starts_with(b"-ERR REPL.WAIT"), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn wait_on_replica_is_rejected() {
        let repl = primary();
        repl.force_replica_flag();
        assert!(matches!(wait_route(&repl, &argv(&["WAIT", "1", "500"])), Route::Local));
        let mut out = Vec::new();
        cmd_wait(&repl, &argv(&["WAIT", "1", "500"]), &mut out);
        assert!(
            out.starts_with(b"-ERR WAIT cannot be used with replica"),
            "{}",
            String::from_utf8_lossy(&out)
        );
    }
}
