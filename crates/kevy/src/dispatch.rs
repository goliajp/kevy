//! The command dispatch table: maps one parsed command to its RESP reply.
//!
//! [`dispatch`] is a thin router that tries each category handler in turn. Each
//! handler (`dispatch_string`, `dispatch_hash`, …) owns a `match` over the verbs
//! it implements and reports whether it handled the command, so no single
//! function carries the whole command set. Command bodies delegate to the
//! helpers in [`crate::cmd`].

use crate::cmd::*;
use kevy_resp::{
    ArgvView, RespVersion, encode_bulk, encode_double, encode_error, encode_integer,
    encode_map_header, encode_null, encode_null_bulk, encode_set_header, encode_simple_string,
};
use kevy_store::{Store, StoreError};

/// Map one command to its RESP reply bytes.
pub fn dispatch<A: ArgvView + ?Sized>(store: &mut Store, args: &A) -> Vec<u8> {
    let mut out = Vec::new();
    dispatch_into(store, args, &mut out);
    out
}

/// Execute `args` against `store`, appending the RESP reply to `out`. Lets a hot
/// caller (the in-order local fast path) write the reply straight into the
/// connection's output buffer — no per-command reply `Vec` alloc, no copy.
pub fn dispatch_into<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    dispatch_with_proto(store, args, out, false);
}

/// RESP3 variant — same OOM bracketing + same V2 body for unmigrated
/// commands; differs only in that a handful of commands
/// (HGETALL → Map, ZSCORE/ZINCRBY → Double, SMEMBERS → Set, …) get a
/// RESP3-shape override before the V2 fallback runs. Pure additive:
/// every V2 reply that hasn't been migrated yet still goes out
/// byte-for-byte identical.
pub fn dispatch_into_resp3<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    dispatch_with_proto(store, args, out, true);
}

/// Shared body: parse verb, OOM-precheck, try the (V3-or-V2) override
/// chain, fall through to the unknown-command error. The `proto_v3`
/// flag picks ONE extra match arm (the RESP3 override) before the
/// existing V2 chain — it doesn't touch the V2 hot path's instruction
/// stream when `proto_v3 == false` (the cmovne is predicted on every
/// pre-HELLO-3 conn).
fn dispatch_with_proto<A: ArgvView + ?Sized>(
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
    // OOM precheck for memory-growing writes only. When `maxmemory == 0` this
    // is a single not-taken branch inside `Store::precheck_for_write`, so the
    // unlimited-mode hot path keeps its perf budget.
    let is_grow = is_growing_write_verb(cmd);
    if is_grow && store.precheck_for_write().is_err() {
        encode_error(out, OOM_ERR);
        return;
    }
    let handled = (proto_v3 && try_resp3_overrides(cmd, store, args, out))
        || dispatch_conn(cmd, args, out)
        || crate::ops::dispatch_ops(cmd, store, args, out)
        || dispatch_string(cmd, store, args, out)
        || crate::dispatch_collections::dispatch_hash(cmd, store, args, out)
        || crate::dispatch_collections::dispatch_list(cmd, store, args, out)
        || dispatch_set(cmd, store, args, out)
        || crate::dispatch_collections::dispatch_zset(cmd, store, args, out)
        || dispatch_generic(cmd, store, args, out)
        || dispatch_multikey_stub(cmd, out);
    if !handled {
        let shown = String::from_utf8_lossy(name);
        encode_error(out, &format!("ERR unknown command '{shown}'"));
        return;
    }
    // Post-write: trim back under `maxmemory` per the active policy. Same
    // cost profile as the precheck — fast when disabled, sample-loop only
    // when the just-finished command actually pushed us over.
    if is_grow {
        store.try_evict_after_write();
    }
}

/// RESP3-shape replies for the commands whose `dispatch_into` output
/// differs from the V2 form. Returns `true` if the cmd matched + the
/// reply was emitted (so the caller skips the V2 chain).
///
/// Adding a new override here is the P3-style migration point: each
/// arm is a 1:1 swap from a V2 helper to a RESP3 helper (Map / Set /
/// Double / Verbatim / …). All other commands keep their V2 wire on
/// RESP3 conns until they get an override — spec-legal gradual
/// migration.
fn try_resp3_overrides<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"HGETALL" => {
            if args.len() != 2 {
                wrong_args(out, "hgetall");
            } else {
                emit_hash_map_resp3(store.hgetall(&args[1]), out);
            }
            true
        }
        b"ZSCORE" => {
            if args.len() != 3 {
                wrong_args(out, "zscore");
            } else {
                emit_zscore_resp3(store.zscore(&args[1], &args[2]), out);
            }
            true
        }
        b"ZINCRBY" => {
            if args.len() != 4 {
                wrong_args(out, "zincrby");
            } else if let Some(incr) = arg_f64(&args[2]) {
                emit_zincrby_resp3(store.zincrby(&args[1], incr, &args[3]), out);
            } else {
                encode_error(out, "ERR value is not a valid float");
            }
            true
        }
        b"SMEMBERS" => {
            if args.len() != 2 {
                wrong_args(out, "smembers");
            } else {
                emit_set_resp3(store.smembers(&args[1]), out);
            }
            true
        }
        b"CONFIG" => {
            // CONFIG GET shape changes RESP2 `*2N` array → RESP3 `%N` Map.
            // Other CONFIG subcommands (SET / REWRITE / RESETSTAT) have
            // the same reply shape under both protos; cmd_config ignores
            // `proto` for those arms. Routing all CONFIG sub-cmds through
            // the V3 path here is simpler than peeking the sub-cmd.
            let cfg = crate::config_global::get();
            crate::ops::config::cmd_config(&cfg, args, out, RespVersion::V3);
            true
        }
        _ => false,
    }
}

/// `HGETALL` over RESP3: flat `[k, v, k, v, ...]` shape from the store
/// becomes a `%N` Map header + N (k, v) pairs.
fn emit_hash_map_resp3(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(flat) => {
            let pairs = flat.len() / 2;
            encode_map_header(out, pairs as i64);
            for v in &flat {
                encode_bulk(out, v);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `SMEMBERS` over RESP3: array of bulk strings becomes a `~N` Set header.
fn emit_set_resp3(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(items) => {
            encode_set_header(out, items.len() as i64);
            for v in &items {
                encode_bulk(out, v);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `ZSCORE` over RESP3: `Some(f)` → `,<f>\r\n` Double; `None` →
/// `_\r\n` RESP3 Null (vs the RESP2 `$-1\r\n` nil bulk).
fn emit_zscore_resp3(res: Result<Option<f64>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(Some(sc)) => encode_double(out, sc),
        Ok(None) => encode_null(out),
        Err(e) => store_err(out, e),
    }
}

/// `ZINCRBY` over RESP3: new score → Double (RESP2 emitted bulk).
fn emit_zincrby_resp3(res: Result<f64, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(sc) => encode_double(out, sc),
        Err(e) => store_err(out, e),
    }
}

/// Connection / introspection commands (no keyspace access).
fn dispatch_conn<A: ArgvView + ?Sized>(cmd: &[u8], args: &A, out: &mut Vec<u8>) -> bool {
    match cmd {
        b"PING" => match args.len() {
            1 => encode_simple_string(out, "PONG"),
            2 => encode_bulk(out, &args[1]),
            _ => wrong_args(out, "ping"),
        },
        b"ECHO" => {
            if args.len() == 2 {
                encode_bulk(out, &args[1]);
            } else {
                wrong_args(out, "echo");
            }
        }
        b"COMMAND" => out.extend_from_slice(b"*0\r\n"),
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
/// This is the v1.0.2 minimal: real multi-DB support (SELECT N + `MOVE` +
/// `SWAPDB` + `databases` config + per-shard `Vec<Store>`) is on the
/// v1.1.0 backlog.
fn cmd_select<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        wrong_args(out, "select");
        return;
    }
    let idx_bytes = &args[1];
    // Redis parses with strtoll-equivalent: leading sign, digits only,
    // no fractional / whitespace. Anything else → "value is not an integer".
    let s = match std::str::from_utf8(idx_bytes) {
        Ok(s) => s,
        Err(_) => {
            encode_error(out, "ERR value is not an integer or out of range");
            return;
        }
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

/// String commands.
fn dispatch_string<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"SET" => cmd_set(store, args, out),
        b"GET" => {
            if args.len() != 2 {
                wrong_args(out, "get");
            } else {
                match store.get(&args[1]) {
                    Ok(Some(v)) => encode_bulk(out, v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            }
        }
        b"APPEND" => {
            if args.len() != 3 {
                wrong_args(out, "append");
            } else {
                emit_int_result(store.append(&args[1], &args[2]).map(|n| n as i64), out);
            }
        }
        b"STRLEN" => {
            if args.len() != 2 {
                wrong_args(out, "strlen");
            } else {
                emit_int_result(store.strlen(&args[1]).map(|n| n as i64), out);
            }
        }
        b"INCR" => cmd_incr(store, args, 1, "incr", out),
        b"DECR" => cmd_incr(store, args, -1, "decr", out),
        b"INCRBY" => cmd_incr_by(store, args, false, "incrby", out),
        b"DECRBY" => cmd_incr_by(store, args, true, "decrby", out),
        b"SETNX" => {
            if args.len() != 3 {
                wrong_args(out, "setnx");
            } else {
                let set = store.set(&args[1], args[2].to_vec(), None, true, false);
                encode_integer(out, set as i64);
            }
        }
        b"SETEX" => cmd_setex(store, args, 1000, "setex", out),
        b"PSETEX" => cmd_setex(store, args, 1, "psetex", out),
        b"GETSET" => {
            if args.len() != 3 {
                wrong_args(out, "getset");
            } else {
                match store.getset(&args[1], args[2].to_vec()) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            }
        }
        b"GETDEL" => {
            if args.len() != 2 {
                wrong_args(out, "getdel");
            } else {
                match store.getdel(&args[1]) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            }
        }
        b"INCRBYFLOAT" => {
            if args.len() != 3 {
                wrong_args(out, "incrbyfloat");
            } else if let Some(d) = arg_f64(&args[2]) {
                match store.incr_by_float(&args[1], d) {
                    Ok(v) => encode_bulk(out, &v),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, "ERR value is not a valid float");
            }
        }
        _ => return false,
    }
    true
}

/// Set commands (single-key; multi-key SINTER/SUNION/SDIFF are runtime gathers).
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
                emit_int_result(store.sadd(&args[1], &rest(args, 2)).map(|n| n as i64), out);
            }
        }
        b"SREM" => {
            if args.len() < 3 {
                wrong_args(out, "srem");
            } else {
                emit_int_result(store.srem(&args[1], &rest(args, 2)).map(|n| n as i64), out);
            }
        }
        b"SCARD" => {
            if args.len() != 2 {
                wrong_args(out, "scard");
            } else {
                emit_int_result(store.scard(&args[1]).map(|n| n as i64), out);
            }
        }
        b"SISMEMBER" => {
            if args.len() != 3 {
                wrong_args(out, "sismember");
            } else {
                emit_int_result(store.sismember(&args[1], &args[2]).map(|b| b as i64), out);
            }
        }
        b"SMEMBERS" => {
            if args.len() != 2 {
                wrong_args(out, "smembers");
            } else {
                emit_bulk_array(store.smembers(&args[1]), out);
            }
        }
        b"SPOP" => cmd_spop_rand(store, args, true, out),
        b"SRANDMEMBER" => cmd_spop_rand(store, args, false, out),
        _ => return false,
    }
    true
}

/// Type-agnostic key commands.
fn dispatch_generic<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"DEL" => {
            if args.len() < 2 {
                wrong_args(out, "del");
            } else {
                encode_integer(out, store.del(&rest(args, 1)) as i64);
            }
        }
        b"EXISTS" => {
            if args.len() < 2 {
                wrong_args(out, "exists");
            } else {
                encode_integer(out, store.exists(&rest(args, 1)) as i64);
            }
        }
        b"EXPIRE" => cmd_expire(store, args, 1000, "expire", out),
        b"PEXPIRE" => cmd_expire(store, args, 1, "pexpire", out),
        b"TTL" => cmd_ttl(store, args, true, "ttl", out),
        b"PTTL" => cmd_ttl(store, args, false, "pttl", out),
        b"PERSIST" => {
            if args.len() != 2 {
                wrong_args(out, "persist");
            } else {
                encode_integer(out, store.persist(&args[1]) as i64);
            }
        }
        b"TYPE" => {
            if args.len() != 2 {
                wrong_args(out, "type");
            } else {
                encode_simple_string(out, store.type_of(&args[1]));
            }
        }
        b"DBSIZE" => encode_integer(out, store.dbsize() as i64),
        b"FLUSHDB" | b"FLUSHALL" => {
            store.flush();
            encode_simple_string(out, "OK");
        }
        _ => return false,
    }
    true
}

/// Multi-key & pub/sub verbs are served by the runtime's cross-shard gather;
/// they only reach `dispatch` when malformed (route fell back to `Local`), so
/// here they just emit the arity error.
fn dispatch_multikey_stub(cmd: &[u8], out: &mut Vec<u8>) -> bool {
    match cmd {
        b"MSET" => wrong_args(out, "mset"),
        b"MGET" => wrong_args(out, "mget"),
        b"SINTER" => wrong_args(out, "sinter"),
        b"SUNION" => wrong_args(out, "sunion"),
        b"SDIFF" => wrong_args(out, "sdiff"),
        b"KEYS" => wrong_args(out, "keys"),
        b"SCAN" => wrong_args(out, "scan"),
        b"RANDOMKEY" => wrong_args(out, "randomkey"),
        b"SUBSCRIBE" => wrong_args(out, "subscribe"),
        b"PUBLISH" => wrong_args(out, "publish"),
        _ => return false,
    }
    true
}
