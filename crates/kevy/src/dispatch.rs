//! The command dispatch table: maps one parsed command to its RESP reply.
//!
//! [`dispatch`] is a thin router that tries each category handler in turn.
//! The single-shard data commands, streams and geo among them, are executed
//! by `kevy_verbs::exec`, the same code the embedded engine runs; what
//! stays here is what only a server has — connection state, the ops and
//! cluster verbs, RESP3 reply shapes, Lua, scope routing and the
//! `maxmemory` bracket.

use crate::cmd::{OOM_ERR, cmd_hello, is_growing_write_verb, store_err, upper_verb, wrong_args};
use crate::state::Ctx;
use kevy_resp::{ArgvView, encode_bulk, encode_error, encode_null_bulk, encode_simple_string};
use kevy_rt::VerbId;
use kevy_store::Store;
use kevy_verbs::Effect;

/// Map one command to its RESP reply bytes.
pub(crate) fn dispatch<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
) -> Vec<u8> {
    let mut out = Vec::new();
    dispatch_into(ctx, store, args, &mut out);
    out
}

/// Execute `args` against `store`, appending the RESP reply to `out`. Lets a hot
/// caller (the in-order local fast path) write the reply straight into the
/// connection's output buffer — no per-command reply `Vec` alloc, no copy.
pub(crate) fn dispatch_into<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    dispatch_with_proto(ctx, store, args, out, false);
}

/// RESP3 variant — same OOM bracketing + same V2 body for unmigrated
/// commands; differs only in that a handful of commands
/// (HGETALL → Map, ZSCORE/ZINCRBY → Double, SMEMBERS → Set, …) get a
/// RESP3-shape override before the V2 fallback runs. Pure additive:
/// every V2 reply that hasn't been migrated yet still goes out
/// byte-for-byte identical.
pub(crate) fn dispatch_into_resp3<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    dispatch_with_proto(ctx, store, args, out, true);
}

/// The ids [`crate::cmd_resolve::kevy_resolve`] hands out. Only the tier-1
/// pair has one: every other verb resolves to [`VerbId::UNKNOWN`] and is
/// matched by name on the executing shard.
pub(crate) const VERB_GET: VerbId = VerbId::new(1);
pub(crate) const VERB_SET: VerbId = VerbId::new(2);

/// [`dispatch_with_proto`] entered with the verb id the origin shard's
/// resolve found: GET and SET go straight to their bodies (neither has a
/// RESP3 override), everything else takes the matching path.
#[inline]
pub(crate) fn dispatch_verb_into<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    verb: VerbId,
    proto_v3: bool,
    out: &mut Vec<u8>,
) {
    if verb == VERB_GET {
        tier1_get(store, args, out);
    } else if verb == VERB_SET {
        if !scope_redirect(ctx, args, out) {
            tier1_set(store, args, out);
        }
    } else {
        dispatch_with_proto(ctx, store, args, out, proto_v3);
    }
}

/// Shared body: parse verb, OOM-precheck, try the (V3-or-V2) override
/// chain, fall through to the unknown-command error. The `proto_v3`
/// flag picks ONE extra match arm (the RESP3 override) before the
/// existing V2 chain — it doesn't touch the V2 hot path's instruction
/// stream when `proto_v3 == false` (the cmovne is predicted on every
/// pre-HELLO-3 conn).
// LOC-WAIVER: per-op dispatch hot body — tier-1 GET/SET fast path + handler chain stay fused.
fn dispatch_with_proto<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto_v3: bool,
) {
    let Some(name) = args.first() else {
        encode_error(out, "ERR empty command");
        return;
    };
    // Case-fold the verb for matching without a per-command heap allocation. A
    // verb longer than the buffer yields an empty slice → no handler matches →
    // the unknown-command error below (which reports the original `name`).
    let mut buf = [0u8; 32];
    let cmd = upper_verb(name, &mut buf);
    // Scope routing. **Above** the GET/SET
    // fast path because SET must respect scope ownership too (the
    // fast path otherwise would silently apply locally). The
    // SCOPE_ACTIVE gate bit is one cached-epoch check + branch —
    // predicted away when no scopes are declared (the scope-free
    // hot path eats one mispredict-resistant load on every command,
    // which is below measurable noise per `bench/perfgate.sh`).
    if crate::cmd::is_write_verb(cmd) && scope_redirect(ctx, args, out) {
        return;
    }
    // Tier-1 fast path: GET / SET are the overwhelming bulk of real traffic;
    // dispatch them in ONE match instead of walking the category-handler
    // chain (conn → ops → string → …) whose every stage re-matches the verb.
    // Neither has a RESP3 override, so this is proto-agnostic. SET keeps the
    // grow-verb OOM bracket (precheck + post-write evict) inline.
    match cmd {
        b"GET" => {
            tier1_get(store, args, out);
            return;
        }
        b"SET" => {
            tier1_set(store, args, out);
            return;
        }
        _ => {}
    }
    // OOM precheck for memory-growing writes only. Gated on the write gate
    // so the default unlimited case skips both calls.
    let is_grow = is_growing_write_verb(cmd);
    if store.precheck_needed() && is_grow && store.precheck_for_write().is_err() {
        encode_error(out, oom_reply(store));
        return;
    }
    let mark = out.len();
    let handled = (proto_v3
        && crate::dispatch_resp3::try_resp3_overrides(ctx, cmd, store, args, out))
        || dispatch_conn(ctx, cmd, store, args, out)
        || crate::ops::dispatch_ops(ctx, cmd, store, args, out)
        || exec_shared(cmd, store, args, out)
        || kevy_verbs::geo::exec_read_only(cmd, store, args, out)
        || internal_record(ctx, cmd, store, args, out)
        // EVAL / EVALSHA / EVAL_RO / EVALSHA_RO / SCRIPT.
        || crate::cmd_lua::dispatch_lua(ctx, cmd, store, args, out)
        || crate::dispatch_replay::dispatch_multikey_stub(cmd, out);
    if !handled {
        crate::cmd::unhandled_verb(out, name, args.len());
        return;
    }
    if proto_v3 {
        kevy_verbs::cmd::stream_resp3(cmd, out, mark);
    }
    // Post-write: trim back under `maxmemory` per the active policy. Gated on
    // both `maxmemory > 0` (the F3 hoist) and `is_grow` so the default unlimited
    // case is two not-taken branches.
    if is_grow && store.maxmemory() > 0 {
        store.try_evict_after_write();
    }
    // Tiering: one budgeted spill batch after a growing write (cheap
    // not-taken branch when tiering is off).
    if is_grow {
        store.try_demote_after_write();
    }
}

/// A write to a key a scope owns elsewhere (or is moving): encode the
/// redirect and report that the command is answered. One cached gate bit
/// when no scope is declared.
#[inline]
fn scope_redirect<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) -> bool {
    if ctx.shard.gate_bits(ctx.state) & crate::state::SCOPE_ACTIVE == 0 {
        return false;
    }
    let Some(redirect) = args.get(1).and_then(|key| ctx.state.route_write(key, ctx.shard)) else {
        return false;
    };
    match redirect {
        crate::state::WriteRedirect::Misdirected(addr) => {
            crate::state::encode_misdirected(out, &addr);
        }
        crate::state::WriteRedirect::Quiesced { to_addr } => {
            crate::state::encode_quiesced(out, &to_addr);
        }
    }
    true
}

#[inline(always)]
fn tier1_get<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() == 2 {
        match store.get(&args[1]) {
            Ok(Some(v)) => encode_bulk(out, &v),
            Ok(None) => encode_null_bulk(out),
            Err(e) => store_err(out, e),
        }
    } else {
        wrong_args(out, "get");
    }
}

#[inline(always)]
fn tier1_set<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    // Hoist the write gate (maxmemory set, or the memory guard refusing)
    // out of the precheck/evict calls so the default case is a single
    // not-taken branch.
    if store.precheck_needed() {
        if store.precheck_for_write().is_err() {
            encode_error(out, oom_reply(store));
            return;
        }
        kevy_verbs::cmd::set(store, args, out);
        store.try_evict_after_write();
    } else {
        kevy_verbs::cmd::set(store, args, out);
    }
    // Tiering's demotion twin: internally gated on `tier.is_some()`.
    store.try_demote_after_write();
}

/// The refusal a growing write gets: the memory guard's when it is the one
/// refusing, Redis's maxmemory reply otherwise.
#[cold]
fn oom_reply(store: &Store) -> &'static str {
    if store.memory_refused() { crate::mem_guard::OVER_BUDGET_ERR } else { OOM_ERR }
}

/// The single-shard data commands, run by the layer the embedded engine
/// shares. A command whose effect is random or clock-bound asks for a
/// different record than its argv; the runtime's post-write step reads that override.
#[inline]
fn exec_shared<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match kevy_verbs::exec(store, cmd, args, out) {
        None => false,
        Some(Effect::Record(frame)) => {
            record_instead(kevy_rt::propagation::Propagate::Replace(frame));
            true
        }
        Some(
            e @ (Effect::RecordId(..)
            | Effect::RecordClaim(_)
            | Effect::RecordRead(..)
            | Effect::RecordReads(_)
            | Effect::RecordHistory(_)
            | Effect::RecordAdd(..)
            | Effect::RecordSeen),
        ) => {
            record_deferred(e);
            true
        }
        Some(Effect::Skip) => {
            record_instead(kevy_rt::propagation::Propagate::Suppress);
            true
        }
        Some(Effect::Read | Effect::Write | Effect::Unchanged) => true,
    }
}

/// An internal record verb: applied when this thread replays the AOF or
/// applies a frame from a primary, refused from a client (a connection,
/// a script, a transaction).
fn internal_record<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    let catalog = cmd == kevy_resp::ops_table::CATALOG.as_bytes();
    if !catalog && cmd != kevy_resp::ops_table::CONSUMER_SEEN.as_bytes() {
        return false;
    }
    if !kevy_rt::applying_record() {
        refuse_internal(out);
    } else if catalog {
        crate::catalog_record::apply(ctx.state, args, out);
    } else {
        kevy_verbs::aof::apply_internal(store, args, out);
    }
    true
}

#[cold]
fn refuse_internal(out: &mut Vec<u8>) {
    encode_error(out, kevy_verbs::aof::INTERNAL_REFUSAL);
}

#[cold]
fn record_instead(p: kevy_rt::propagation::Propagate) {
    kevy_rt::propagation::set_override(p);
}

/// A record the runtime builds only if it records the write: nothing is
/// built with the AOF off and no replicas.
#[cold]
fn record_deferred(effect: Effect) {
    kevy_rt::propagation::set_override_deferred(effect);
}

/// Record an `SPOP` by the members it removed, for a reply path that pops
/// outside [`kevy_verbs::exec`]; an empty pop records nothing.
pub(crate) fn record_spop(key: &[u8], popped: &[Vec<u8>]) {
    record_instead(if popped.is_empty() {
        kevy_rt::propagation::Propagate::Suppress
    } else {
        let frame = kevy_verbs::aof::spop_effect(key, popped);
        kevy_rt::propagation::Propagate::Replace(frame.into_iter().map(<[u8]>::to_vec).collect())
    });
}

// `try_resp3_overrides` + the `emit_*_resp3` helpers live in
// [`crate::dispatch_resp3`] — split out so this file stays under the
// 500-LOC house rule. Same dispatch fan-out, same call shape; the
// V3 arm in `dispatch_with_proto` calls into the sibling module.

/// `TIME` — the clock, and nothing else: no key, no shard, which is why
/// it sits with the introspection verbs rather than the keyspace ones.
/// Redis answers a two-element array of decimal strings: unix seconds,
/// then the microseconds within that second.
fn cmd_time(out: &mut Vec<u8>) {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    kevy_resp::encode_array_len(out, 2);
    encode_bulk(out, now.as_secs().to_string().as_bytes());
    encode_bulk(out, now.subsec_micros().to_string().as_bytes());
}

/// Connection / introspection commands (no keyspace access — except
/// IDX.CREATE's tiering-floor precheck, which reads the answering
/// shard's tier gauges). Takes `ctx` for the catalog-mutation verbs
/// (IDX.* / VIEW.*), whose sidecar persistence roots at
/// `state.sidecar_dir()`.
fn dispatch_conn<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"PING" => match args.len() {
            1 => encode_simple_string(out, "PONG"),
            2 => encode_bulk(out, &args[1]),
            _ => wrong_args(out, "ping"),
        },
        b"TIME" => cmd_time(out),
        b"IDX.CREATE" | b"IDX.DROP" | b"VIEW.CREATE" | b"VIEW.DROP" | b"TABLE.DECLARE"
        | b"TABLE.ENSURE" | b"TABLE.REPLACE" | b"TABLE.DROP" => {
            return crate::catalog_record::dispatch(ctx, cmd, store, args, out);
        }
        b"IDX.ADVISE" => crate::cmd_index_advise::cmd_idx_advise(ctx, args, out),
        b"TABLE.DESCRIBE" => crate::cmd_describe::cmd_table_describe(ctx, args, out),
        b"IDX.DESCRIBE" => crate::cmd_describe::cmd_idx_describe(ctx, args, out),
        b"VIEW.DESCRIBE" => crate::cmd_describe::cmd_view_describe(ctx, args, out),
        // Well-formed LIST/VERIFY ride the extension fan-out; only a
        // malformed arity falls through to these usage arms.
        b"TABLE.LIST" => encode_error(out, "ERR usage: TABLE.LIST"),
        b"TABLE.VERIFY" => encode_error(out, "ERR usage: TABLE.VERIFY name"),
        b"ECHO" => {
            if args.len() == 2 {
                encode_bulk(out, &args[1]);
            } else {
                wrong_args(out, "echo");
            }
        }
        b"COMMAND" => crate::cmd_command::cmd_command(args, out),
        b"FAILOVER" => crate::cmd_failover::cmd_failover(ctx, args, out),
        b"HELLO" => cmd_hello(out),
        b"QUIT" => encode_simple_string(out, "OK"),
        // CONFIG moved to crate::ops::dispatch_ops (real GET reads Config;
        // SET / REWRITE return helpful errors until v1.x).
        b"SELECT" => cmd_select(args, out),
        _ => return false,
    }
    true
}

/// `SELECT <index>` — single-DB acknowledgement.
///
/// kevy is a single-database server (one keyspace per shard pool, no
/// `databases N` config). For drop-in client compatibility we accept
/// `SELECT 0` (the Redis default) with `+OK` and reject any other index
/// with the byte-identical Redis error.
///
/// Real multi-DB support (SELECT N + `MOVE` + `SWAPDB` + `databases`
/// config + per-shard `Vec<Store>`) is intentionally not implemented.
fn cmd_select<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        wrong_args(out, "select");
        return;
    }
    let idx_bytes = &args[1];
    // Redis parses with strtoll-equivalent: leading sign, digits only,
    // no fractional / whitespace. Anything else → "value is not an integer".
    let Ok(s) = std::str::from_utf8(idx_bytes) else {
        encode_error(out, "ERR value is not an integer or out of range");
        return;
    };
    let parsed: Result<i64, _> = s.parse();
    match parsed {
        Ok(0) => encode_simple_string(out, "OK"),
        // Explicit: kevy is single-DB (unlike valkey's default 16). Tell the
        // caller *why* it's rejected so they don't assume it's an arbitrary
        // index out-of-range that they could config their way around.
        Ok(_) => encode_error(
            out,
            "ERR kevy only supports DB 0 (multi-database support is on the v1.1.0 backlog)",
        ),
        // Byte-identical to valkey's "value is not an integer or out of range"
        // — this one is a real parser error, not a kevy-specific limit.
        Err(_) => encode_error(out, "ERR value is not an integer or out of range"),
    }
}
