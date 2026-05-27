//! The command dispatch table: maps one parsed command to its RESP reply.
//!
//! [`dispatch`] is a thin router that tries each category handler in turn. Each
//! handler (`dispatch_string`, `dispatch_hash`, …) owns a `match` over the verbs
//! it implements and reports whether it handled the command, so no single
//! function carries the whole command set. Command bodies delegate to the
//! helpers in [`crate::cmd`].

use crate::cmd::*;
use kevy_resp::{
    Argv, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
use kevy_store::Store;

/// Map one command to its RESP reply bytes.
pub fn dispatch(store: &mut Store, args: &Argv) -> Vec<u8> {
    let mut out = Vec::new();
    dispatch_into(store, args, &mut out);
    out
}

/// Execute `args` against `store`, appending the RESP reply to `out`. Lets a hot
/// caller (the in-order local fast path) write the reply straight into the
/// connection's output buffer — no per-command reply `Vec` alloc, no copy.
pub fn dispatch_into(store: &mut Store, args: &Argv, out: &mut Vec<u8>) {
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
    // unlimited-mode hot path keeps its v0.metal cycle budget.
    let is_grow = is_growing_write_verb(cmd);
    if is_grow && store.precheck_for_write().is_err() {
        encode_error(out, OOM_ERR);
        return;
    }
    let handled = dispatch_conn(cmd, args, out)
        || crate::ops::dispatch_ops(cmd, store, args, out)
        || dispatch_string(cmd, store, args, out)
        || dispatch_hash(cmd, store, args, out)
        || dispatch_list(cmd, store, args, out)
        || dispatch_set(cmd, store, args, out)
        || dispatch_zset(cmd, store, args, out)
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

/// Connection / introspection commands (no keyspace access).
fn dispatch_conn(cmd: &[u8], args: &Argv, out: &mut Vec<u8>) -> bool {
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
        _ => return false,
    }
    true
}

/// String commands.
fn dispatch_string(cmd: &[u8], store: &mut Store, args: &Argv, out: &mut Vec<u8>) -> bool {
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

/// Hash commands.
fn dispatch_hash(cmd: &[u8], store: &mut Store, args: &Argv, out: &mut Vec<u8>) -> bool {
    match cmd {
        b"HSET" => cmd_hset(store, args, out),
        b"HSETNX" => {
            if args.len() != 4 {
                wrong_args(out, "hsetnx");
            } else {
                emit_int_result(
                    store.hsetnx(&args[1], &args[2], &args[3]).map(|b| b as i64),
                    out,
                );
            }
        }
        b"HGET" => {
            if args.len() != 3 {
                wrong_args(out, "hget");
            } else {
                match store.hget(&args[1], &args[2]) {
                    Ok(Some(v)) => encode_bulk(out, v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            }
        }
        b"HDEL" => {
            if args.len() < 3 {
                wrong_args(out, "hdel");
            } else {
                emit_int_result(store.hdel(&args[1], &rest(args, 2)).map(|n| n as i64), out);
            }
        }
        b"HEXISTS" => {
            if args.len() != 3 {
                wrong_args(out, "hexists");
            } else {
                emit_int_result(store.hexists(&args[1], &args[2]).map(|b| b as i64), out);
            }
        }
        b"HLEN" => {
            if args.len() != 2 {
                wrong_args(out, "hlen");
            } else {
                emit_int_result(store.hlen(&args[1]).map(|n| n as i64), out);
            }
        }
        b"HINCRBY" => {
            if args.len() != 4 {
                wrong_args(out, "hincrby");
            } else if let Some(d) = arg_i64(&args[3]) {
                emit_int_result(store.hincrby(&args[1], &args[2], d), out);
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"HKEYS" => {
            if args.len() != 2 {
                wrong_args(out, "hkeys");
            } else {
                emit_bulk_array(store.hkeys(&args[1]), out);
            }
        }
        b"HVALS" => {
            if args.len() != 2 {
                wrong_args(out, "hvals");
            } else {
                emit_bulk_array(store.hvals(&args[1]), out);
            }
        }
        b"HGETALL" => {
            if args.len() != 2 {
                wrong_args(out, "hgetall");
            } else {
                emit_bulk_array(store.hgetall(&args[1]), out);
            }
        }
        b"HMGET" => {
            if args.len() < 3 {
                wrong_args(out, "hmget");
            } else {
                match store.hmget(&args[1], &rest(args, 2)) {
                    Ok(vals) => {
                        encode_array_len(out, vals.len() as i64);
                        for v in &vals {
                            match v {
                                Some(b) => encode_bulk(out, b),
                                None => encode_null_bulk(out),
                            }
                        }
                    }
                    Err(e) => store_err(out, e),
                }
            }
        }
        _ => return false,
    }
    true
}

/// List commands.
fn dispatch_list(cmd: &[u8], store: &mut Store, args: &Argv, out: &mut Vec<u8>) -> bool {
    match cmd {
        b"LPUSH" => {
            if args.len() < 3 {
                wrong_args(out, "lpush");
            } else {
                emit_int_result(store.lpush(&args[1], &rest(args, 2)).map(|n| n as i64), out);
            }
        }
        b"RPUSH" => {
            if args.len() < 3 {
                wrong_args(out, "rpush");
            } else {
                emit_int_result(store.rpush(&args[1], &rest(args, 2)).map(|n| n as i64), out);
            }
        }
        b"LPOP" => cmd_pop(store, args, false, out),
        b"RPOP" => cmd_pop(store, args, true, out),
        b"LLEN" => {
            if args.len() != 2 {
                wrong_args(out, "llen");
            } else {
                emit_int_result(store.llen(&args[1]).map(|n| n as i64), out);
            }
        }
        b"LINDEX" => {
            if args.len() != 3 {
                wrong_args(out, "lindex");
            } else if let Some(i) = arg_i64(&args[2]) {
                match store.lindex(&args[1], i) {
                    Ok(Some(v)) => encode_bulk(out, &v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LRANGE" => {
            if args.len() != 4 {
                wrong_args(out, "lrange");
            } else if let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) {
                emit_bulk_array(store.lrange(&args[1], s, e), out);
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LSET" => {
            if args.len() != 4 {
                wrong_args(out, "lset");
            } else if let Some(i) = arg_i64(&args[2]) {
                match store.lset(&args[1], i, &args[3]) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LREM" => {
            if args.len() != 4 {
                wrong_args(out, "lrem");
            } else if let Some(c) = arg_i64(&args[2]) {
                emit_int_result(store.lrem(&args[1], c, &args[3]).map(|n| n as i64), out);
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        b"LTRIM" => {
            if args.len() != 4 {
                wrong_args(out, "ltrim");
            } else if let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) {
                match store.ltrim(&args[1], s, e) {
                    Ok(()) => encode_simple_string(out, "OK"),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_INT);
            }
        }
        _ => return false,
    }
    true
}

/// Set commands (single-key; multi-key SINTER/SUNION/SDIFF are runtime gathers).
fn dispatch_set(cmd: &[u8], store: &mut Store, args: &Argv, out: &mut Vec<u8>) -> bool {
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

/// Sorted-set commands.
fn dispatch_zset(cmd: &[u8], store: &mut Store, args: &Argv, out: &mut Vec<u8>) -> bool {
    match cmd {
        b"ZADD" => cmd_zadd(store, args, out),
        b"ZSCORE" => {
            if args.len() != 3 {
                wrong_args(out, "zscore");
            } else {
                match store.zscore(&args[1], &args[2]) {
                    Ok(Some(sc)) => encode_bulk(out, &fmt_score(sc)),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            }
        }
        b"ZCARD" => {
            if args.len() != 2 {
                wrong_args(out, "zcard");
            } else {
                emit_int_result(store.zcard(&args[1]).map(|n| n as i64), out);
            }
        }
        b"ZREM" => {
            if args.len() < 3 {
                wrong_args(out, "zrem");
            } else {
                emit_int_result(store.zrem(&args[1], &rest(args, 2)).map(|n| n as i64), out);
            }
        }
        b"ZRANK" => {
            if args.len() != 3 {
                wrong_args(out, "zrank");
            } else {
                match store.zrank(&args[1], &args[2]) {
                    Ok(Some(r)) => encode_integer(out, r as i64),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            }
        }
        b"ZINCRBY" => {
            if args.len() != 4 {
                wrong_args(out, "zincrby");
            } else if let Some(incr) = arg_f64(&args[2]) {
                match store.zincrby(&args[1], incr, &args[3]) {
                    Ok(sc) => encode_bulk(out, &fmt_score(sc)),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, "ERR value is not a valid float");
            }
        }
        b"ZRANGE" => cmd_zrange(store, args, out),
        b"ZRANGEBYSCORE" => cmd_zrangebyscore(store, args, out),
        b"ZCOUNT" => {
            if args.len() != 4 {
                wrong_args(out, "zcount");
            } else if let (Some(min), Some(max)) =
                (parse_score_bound(&args[2]), parse_score_bound(&args[3]))
            {
                emit_int_result(store.zcount(&args[1], min, max).map(|n| n as i64), out);
            } else {
                encode_error(out, "ERR min or max is not a float");
            }
        }
        _ => return false,
    }
    true
}

/// Type-agnostic key commands.
fn dispatch_generic(cmd: &[u8], store: &mut Store, args: &Argv, out: &mut Vec<u8>) -> bool {
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
