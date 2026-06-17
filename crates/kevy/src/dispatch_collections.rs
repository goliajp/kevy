//! Dispatch tables for the three "compound" data types — hash, list,
//! sorted set. The string + set + generic + connection tables live in
//! [`crate::dispatch`] alongside the main `dispatch_into` router; this
//! split keeps each file under the 500-LOC house rule.
//!
//! Each handler is a pure dispatch-table function (CLAUDE.md's listed
//! `match`-table exception to the 50-LOC fn cap): it owns one `match`
//! over the verbs it implements, delegates to a `cmd::*` helper or a
//! direct `store::*` call, and returns whether the verb was handled.

use crate::cmd::{cmd_hset, wrong_args, emit_int_result, store_err, rest, arg_i64, ERR_NOT_INT, emit_bulk_array, cmd_pop, cmd_blpop, cmd_zadd, fmt_score, arg_f64, cmd_zrange, cmd_zrangebyscore, parse_score_bound};
use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
use kevy_store::Store;

/// Hash commands.
pub(crate) fn dispatch_hash<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"HSET" => cmd_hset(store, args, out),
        b"HSETNX" => {
            if args.len() == 4 {
                emit_int_result(
                    store.hsetnx(&args[1], &args[2], &args[3]).map(i64::from),
                    out,
                );
            } else {
                wrong_args(out, "hsetnx");
            }
        }
        b"HGET" => {
            if args.len() == 3 {
                match store.hget(&args[1], &args[2]) {
                    Ok(Some(v)) => encode_bulk(out, v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "hget");
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
            if args.len() == 3 {
                emit_int_result(store.hexists(&args[1], &args[2]).map(i64::from), out);
            } else {
                wrong_args(out, "hexists");
            }
        }
        b"HLEN" => {
            if args.len() == 2 {
                emit_int_result(store.hlen(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "hlen");
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
            if args.len() == 2 {
                emit_bulk_array(store.hkeys(&args[1]), out);
            } else {
                wrong_args(out, "hkeys");
            }
        }
        b"HVALS" => {
            if args.len() == 2 {
                emit_bulk_array(store.hvals(&args[1]), out);
            } else {
                wrong_args(out, "hvals");
            }
        }
        b"HGETALL" => {
            if args.len() == 2 {
                emit_bulk_array(store.hgetall(&args[1]), out);
            } else {
                wrong_args(out, "hgetall");
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
pub(crate) fn dispatch_list<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
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
        b"BLPOP" => cmd_blpop(store, args, false, out),
        b"BRPOP" => cmd_blpop(store, args, true, out),
        b"LLEN" => {
            if args.len() == 2 {
                emit_int_result(store.llen(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "llen");
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

/// Sorted-set commands.
pub(crate) fn dispatch_zset<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"ZADD" => cmd_zadd(store, args, out),
        b"ZSCORE" => {
            if args.len() == 3 {
                match store.zscore(&args[1], &args[2]) {
                    Ok(Some(sc)) => encode_bulk(out, &fmt_score(sc)),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "zscore");
            }
        }
        b"ZCARD" => {
            if args.len() == 2 {
                emit_int_result(store.zcard(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "zcard");
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
            if args.len() == 3 {
                match store.zrank(&args[1], &args[2]) {
                    Ok(Some(r)) => encode_integer(out, r as i64),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "zrank");
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
        b"ZRANGE" => cmd_zrange(store, args, out, kevy_resp::RespVersion::V2),
        b"ZRANGEBYSCORE" => cmd_zrangebyscore(store, args, out, kevy_resp::RespVersion::V2),
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
