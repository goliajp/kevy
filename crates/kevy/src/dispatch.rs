//! The command dispatch table: maps one parsed command to its RESP reply.
//!
//! [`dispatch`] is a thin router that tries each category handler in turn. Each
//! handler (`dispatch_string`, `dispatch_hash`, …) owns a `match` over the verbs
//! it implements and reports whether it handled the command, so no single
//! function carries the whole command set. Command bodies delegate to the
//! helpers in [`crate::cmd`].

use crate::cmd::{
    OOM_ERR, cmd_expire, cmd_expireat, cmd_hello, cmd_set, cmd_spop_rand, cmd_ttl, emit_bulk_array,
    emit_int_result, is_growing_write_verb, rest_borrowed, store_err, upper_verb, wrong_args,
};
use crate::state::Ctx;
use kevy_resp::{
    ArgvView, encode_bulk, encode_error, encode_integer, encode_null_bulk, encode_simple_string,
};
use kevy_store::Store;

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
    if crate::cmd::is_write_verb(cmd)
        && ctx.shard.gate_bits(ctx.state) & crate::state::SCOPE_ACTIVE != 0
        && let Some(key) = args.get(1)
        && let Some(redirect) = ctx.state.route_write(key, ctx.shard)
    {
        match redirect {
            crate::state::WriteRedirect::Misdirected(addr) => {
                crate::state::encode_misdirected(out, &addr);
            }
            crate::state::WriteRedirect::Quiesced { to_addr } => {
                crate::state::encode_quiesced(out, &to_addr);
            }
        }
        return;
    }
    // Tier-1 fast path: GET / SET are the overwhelming bulk of real traffic;
    // dispatch them in ONE match instead of walking the category-handler
    // chain (conn → ops → string → …) whose every stage re-matches the verb.
    // Neither has a RESP3 override, so this is proto-agnostic. SET keeps the
    // grow-verb OOM bracket (precheck + post-write evict) inline.
    match cmd {
        b"GET" => {
            if args.len() == 2 {
                match store.get(&args[1]) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "get");
            }
            return;
        }
        b"SET" => {
            // Hoist the maxmemory gate out of the precheck/evict
            // function calls so the default `maxmemory=0` case is a single
            // not-taken branch right here, skipping two `#[inline]` function
            // invocations + their internal branches.
            if store.maxmemory() > 0 {
                if store.precheck_for_write().is_err() {
                    encode_error(out, OOM_ERR);
                    return;
                }
                cmd_set(store, args, out);
                store.try_evict_after_write();
            } else {
                cmd_set(store, args, out);
            }
            // Tiering's demotion twin: internally gated on
            // `tier.is_some()` — one not-taken branch when off.
            store.try_demote_after_write();
            return;
        }
        _ => {}
    }
    // OOM precheck for memory-growing writes only. Gated on `maxmemory > 0`
    // so the default unlimited case skips both calls.
    let is_grow = is_growing_write_verb(cmd);
    if store.maxmemory() > 0 && is_grow && store.precheck_for_write().is_err() {
        encode_error(out, OOM_ERR);
        return;
    }
    let handled = (proto_v3
        && crate::dispatch_resp3::try_resp3_overrides(ctx, cmd, store, args, out))
        || dispatch_conn(ctx, cmd, store, args, out)
        || crate::ops::dispatch_ops(ctx, cmd, store, args, out)
        || crate::dispatch_strings::dispatch_string(cmd, store, args, out)
        || crate::dispatch_bitmap::dispatch_bitmap(cmd, store, args, out)
        || crate::dispatch_collections::dispatch_hash(cmd, store, args, out)
        || crate::dispatch_collections::dispatch_list(cmd, store, args, out)
        || dispatch_set(cmd, store, args, out)
        || crate::dispatch_collections::dispatch_zset(cmd, store, args, out)
        || crate::dispatch_geo::dispatch_geo(cmd, store, args, out)
        || crate::dispatch_stream::dispatch_stream(cmd, store, args, out)
        // EVAL / EVALSHA / EVAL_RO / EVALSHA_RO / SCRIPT.
        || crate::cmd_lua::dispatch_lua(ctx, cmd, store, args, out)
        || dispatch_generic(cmd, store, args, out)
        || crate::dispatch_replay::dispatch_multikey_stub(cmd, store, args, out);
    if !handled {
        crate::cmd::unhandled_verb(out, name, args.len());
        return;
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
    store: &Store,
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
        b"IDX.CREATE" => crate::cmd_index::cmd_idx_create(ctx, store, args, out),
        b"VIEW.CREATE" => crate::cmd_view::cmd_view_create(ctx, args, out),
        b"VIEW.DROP" => crate::cmd_view::cmd_view_drop(ctx, args, out),
        b"IDX.DROP" => crate::cmd_index::cmd_idx_drop(ctx, args, out),
        b"IDX.ADVISE" => crate::cmd_index_advise::cmd_idx_advise(ctx, args, out),
        b"TABLE.DECLARE" => crate::cmd_table::cmd_table_declare(ctx, store, args, out),
        b"TABLE.ENSURE" => crate::cmd_table::cmd_table_ensure(ctx, store, args, out),
        b"TABLE.REPLACE" => crate::cmd_table::cmd_table_replace(ctx, store, args, out),
        b"TABLE.DROP" => crate::cmd_table::cmd_table_drop(ctx, args, out),
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

/// Set commands (single-key; multi-key SINTER/SUNION/SDIFF are runtime gathers).
// LOC-WAIVER: data-driven verb dispatch table — one arm per set verb.
fn dispatch_set<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"SADD" => {
            if args.len() < 3 {
                wrong_args(out, "sadd");
            } else {
                emit_int_result(
                    store.sadd(&args[1], &rest_borrowed(args, 2)).map(|n| n as i64),
                    out,
                );
            }
        }
        b"SREM" => {
            if args.len() < 3 {
                wrong_args(out, "srem");
            } else {
                emit_int_result(
                    store.srem(&args[1], &rest_borrowed(args, 2)).map(|n| n as i64),
                    out,
                );
            }
        }
        b"SCARD" => {
            if args.len() == 2 {
                emit_int_result(store.scard(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "scard");
            }
        }
        b"SISMEMBER" => {
            if args.len() == 3 {
                emit_int_result(store.sismember(&args[1], &args[2]).map(i64::from), out);
            } else {
                wrong_args(out, "sismember");
            }
        }
        b"SMEMBERS" => {
            if args.len() == 2 {
                emit_bulk_array(store.smembers(&args[1]), out);
            } else {
                wrong_args(out, "smembers");
            }
        }
        b"SPOP" => cmd_spop_rand(store, args, true, out),
        b"SRANDMEMBER" => cmd_spop_rand(store, args, false, out),
        _ => return false,
    }
    true
}

/// Type-agnostic key commands.
// LOC-WAIVER: data-driven verb dispatch table — one arm per generic verb.
fn dispatch_generic<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        // UNLINK is the Redis 4.0+ async-delete variant; kevy's
        // single-thread-per-shard model makes the "async" part
        // moot (no other reactor steps happen during DEL), so it
        // aliases DEL byte-for-byte. Sidekiq's heartbeat depends
        // on it.
        b"DEL" | b"UNLINK" => {
            let verb = if cmd == b"DEL" { "del" } else { "unlink" };
            if args.len() < 2 {
                wrong_args(out, verb);
            } else {
                encode_integer(out, store.del(&rest_borrowed(args, 1)) as i64);
            }
        }
        // TOUCH counts the keys that exist, and the existence check is
        // what refreshes the eviction bookkeeping on each owning shard
        // — so in this engine it IS `exists`, which is what the embedded
        // facade's `touch` says in one line (`ops_keyspace.rs`, it calls
        // `self.exists`). Two arms answering identically would be two
        // places to keep identical.
        b"EXISTS" | b"TOUCH" => {
            let verb = if cmd == b"EXISTS" { "exists" } else { "touch" };
            if args.len() < 2 {
                wrong_args(out, verb);
            } else {
                encode_integer(out, store.exists(&rest_borrowed(args, 1)) as i64);
            }
        }
        b"EXPIRE" => cmd_expire(store, args, 1000, "expire", out),
        b"PEXPIRE" => cmd_expire(store, args, 1, "pexpire", out),
        b"EXPIREAT" => cmd_expireat(store, args, 1000, "expireat", out),
        b"PEXPIREAT" => cmd_expireat(store, args, 1, "pexpireat", out),
        b"TTL" => cmd_ttl(store, args, true, "ttl", out),
        b"PTTL" => cmd_ttl(store, args, false, "pttl", out),
        b"PERSIST" => {
            if args.len() == 2 {
                encode_integer(out, i64::from(store.persist(&args[1])));
            } else {
                wrong_args(out, "persist");
            }
        }
        b"TYPE" => {
            if args.len() == 2 {
                encode_simple_string(out, store.type_of(&args[1]));
            } else {
                wrong_args(out, "type");
            }
        }
        b"DBSIZE" => encode_integer(out, store.dbsize() as i64),
        b"FLUSHDB" | b"FLUSHALL" => {
            store.flushall();
            encode_simple_string(out, "OK");
        }
        _ => return false,
    }
    true
}
