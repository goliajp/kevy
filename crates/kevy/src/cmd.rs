//! Command helpers shared by the dispatcher.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_double, encode_error,
    encode_integer,
};
use kevy_rt::NotifyClass;
use kevy_store::{ScoreBound, Store, StoreError};

/// Uppercase a command verb into the caller's stack buffer — no per-command heap
/// allocation (verbs are short). An over-long token yields an empty slice, which
/// matches no command literal (i.e. it is treated as unknown — the correct
/// behavior for routing, write-classification, and txn-classification).
pub(crate) fn upper_verb<'a>(name: &[u8], buf: &'a mut [u8; 32]) -> &'a [u8] {
    let n = name.len();
    if n <= buf.len() {
        buf[..n].copy_from_slice(name);
        buf[..n].make_ascii_uppercase();
        &buf[..n]
    } else {
        &buf[..0]
    }
}

pub(crate) fn wrong_args(out: &mut Vec<u8>, cmd: &str) {
    encode_error(
        out,
        &format!("ERR wrong number of arguments for '{cmd}' command"),
    );
}

/// `HELLO` — RESP2 server-info handshake (a flat field/value array). We always
/// report `proto 2`; switching to a true RESP3 reply encoding is deferred.
pub(crate) fn cmd_hello(out: &mut Vec<u8>) {
    encode_array_len(out, 14);
    encode_bulk(out, b"server");
    encode_bulk(out, b"kevy");
    encode_bulk(out, b"version");
    encode_bulk(out, env!("CARGO_PKG_VERSION").as_bytes());
    encode_bulk(out, b"proto");
    encode_integer(out, 2);
    encode_bulk(out, b"id");
    encode_integer(out, 0);
    encode_bulk(out, b"mode");
    encode_bulk(out, b"standalone");
    encode_bulk(out, b"role");
    encode_bulk(out, b"master");
    encode_bulk(out, b"modules");
    encode_array_len(out, 0);
}

pub(crate) const ERR_NOT_INT: &str = "ERR value is not an integer or out of range";
pub(crate) const WRONGTYPE: &str =
    "WRONGTYPE Operation against a key holding the wrong kind of value";
/// Redis's classic OOM reply for write attempts under `NoEviction`. Matches
/// the wording valkey clients (redis-cli, jedis, go-redis) detect.
pub(crate) const OOM_ERR: &str =
    "OOM command not allowed when used memory > 'maxmemory'.";

/// Verb-level "is this a write" classification. Mirrors the `is_write` arm in
/// [`crate::KevyCommands::resolve`] so the local dispatch fast path and the
/// runtime see the same set; both must include every command that can grow
/// `used_memory`, so eviction gates them all. Kept in a single place to avoid
/// drift.
pub(crate) fn is_write_verb(cmd: &[u8]) -> bool {
    matches!(
        cmd,
        b"SET"
            | b"SETNX"
            | b"SETEX"
            | b"PSETEX"
            | b"GETSET"
            | b"GETDEL"
            | b"INCRBYFLOAT"
            | b"DEL"
            | b"INCR"
            | b"DECR"
            | b"INCRBY"
            | b"DECRBY"
            | b"APPEND"
            | b"EXPIRE"
            | b"PEXPIRE"
            | b"EXPIREAT"
            | b"PEXPIREAT"
            | b"PERSIST"
            | b"FLUSHDB"
            | b"FLUSHALL"
            | b"HSET"
            | b"HSETNX"
            | b"HDEL"
            | b"HINCRBY"
            | b"LPUSH"
            | b"RPUSH"
            | b"LPOP"
            | b"RPOP"
            | b"LSET"
            | b"LREM"
            | b"LTRIM"
            | b"SADD"
            | b"SREM"
            | b"SPOP"
            | b"ZADD"
            | b"ZREM"
            | b"ZINCRBY"
            | b"GEOADD"
            | b"GEOSEARCHSTORE"
            | b"GEORADIUS"
            | b"GEORADIUSBYMEMBER"
            | b"XADD"
            | b"XDEL"
            | b"XTRIM"
            | b"XSETID"
            | b"XGROUP"
            | b"XREADGROUP"
            | b"XACK"
            | b"XCLAIM"
            | b"XAUTOCLAIM"
            | b"MSET"
    )
}

/// Classify an uppercased verb into a keyspace-notification class. Returns
/// `None` for read-only / non-notifying commands so the runtime can
/// short-circuit; otherwise a [`NotifyClass`] the caller matches against
/// `NotificationFlags` to decide whether to actually publish.
///
/// Event name = lowercased verb (matches the Redis events.c naming
/// convention — what redis-cli's `PSUBSCRIBE __keyevent@0__:*` reports).
/// Multi-key cmds (DEL multi / MSET / FLUSHDB) get their own per-Op
/// hooks (`maybe_notify_del` / `maybe_notify_mset` / `maybe_notify_flush`
/// in `kevy-rt::exec_notify`); this table covers single-key dispatch only.
pub(crate) fn notify_class_for_verb(cmd: &[u8]) -> Option<NotifyClass> {
    Some(match cmd {
        // String — Redis class `$`.
        b"SET" | b"SETNX" | b"SETEX" | b"PSETEX" | b"GETSET" | b"GETDEL"
        | b"APPEND" | b"INCR" | b"DECR" | b"INCRBY" | b"DECRBY" | b"INCRBYFLOAT" => {
            NotifyClass::String
        }
        // Hash — class `h`.
        b"HSET" | b"HSETNX" | b"HDEL" | b"HINCRBY" => NotifyClass::Hash,
        // List — class `l`.
        b"LPUSH" | b"RPUSH" | b"LPOP" | b"RPOP" | b"LSET" | b"LREM" | b"LTRIM" => {
            NotifyClass::List
        }
        // Set — class `s` (SINTERSTORE/SUNIONSTORE/SDIFFSTORE not yet impl'd).
        b"SADD" | b"SREM" | b"SPOP" => NotifyClass::Set,
        // Sorted set — class `z`. GEOADD writes a ZSet under the hood,
        // so it fires `zadd` notifications too (matches Redis).
        b"ZADD" | b"ZREM" | b"ZINCRBY" | b"GEOADD" => NotifyClass::Zset,
        // Stream — class `t`. XADD/XDEL/XTRIM/XGROUP/XACK/XCLAIM/
        // XREADGROUP all fire their lowercased verb name.
        b"XADD" | b"XDEL" | b"XTRIM" | b"XSETID" | b"XGROUP" | b"XACK" | b"XCLAIM"
        | b"XAUTOCLAIM" | b"XREADGROUP" => NotifyClass::Stream,
        // Generic — class `g`. (DEL single-key falls here; multi-key DEL
        // is routed through Op::Del + maybe_notify_del directly.)
        b"DEL" | b"EXPIRE" | b"PEXPIRE" | b"PERSIST" => NotifyClass::Generic,
        // Reads, admin, pub/sub etc. — no notification.
        _ => return None,
    })
}

/// Subset of [`is_write_verb`] that can *grow* memory. `DEL` / `HDEL` / `LPOP`
/// / `LREM` / `LTRIM` / `SREM` / `ZREM` / `EXPIRE` / `PERSIST` are writes but
/// only ever shrink (or hold steady), so they never need the OOM precheck —
/// and `FLUSH*` actively rescues us from OOM. Keeping them out of the precheck
/// list lets a NoEviction-configured shard always accept shrinkers, matching
/// Redis exactly.
pub(crate) fn is_growing_write_verb(cmd: &[u8]) -> bool {
    matches!(
        cmd,
        b"SET"
            | b"SETNX"
            | b"SETEX"
            | b"PSETEX"
            | b"GETSET"
            | b"INCRBYFLOAT"
            | b"INCR"
            | b"DECR"
            | b"INCRBY"
            | b"DECRBY"
            | b"APPEND"
            | b"HSET"
            | b"HSETNX"
            | b"HINCRBY"
            | b"LPUSH"
            | b"RPUSH"
            | b"LSET"
            | b"SADD"
            | b"ZADD"
            | b"ZINCRBY"
            | b"GEOADD"
            | b"GEOSEARCHSTORE"
            | b"GEORADIUS"
            | b"GEORADIUSBYMEMBER"
            | b"XADD"
            | b"XGROUP"
            | b"XREADGROUP"
            | b"XCLAIM"
            | b"XAUTOCLAIM"
            | b"MSET"
    )
}

/// Encode a `StoreError` as its RESP error reply.
pub(crate) fn store_err(out: &mut Vec<u8>, e: StoreError) {
    let msg = match e {
        StoreError::WrongType => WRONGTYPE,
        StoreError::NotInteger => ERR_NOT_INT,
        StoreError::Overflow => "ERR increment or decrement would overflow",
        StoreError::OutOfRange => "ERR index out of range",
        StoreError::NoSuchKey => "ERR no such key",
        StoreError::NotFloat => "ERR value is not a valid float",
        StoreError::OutOfMemory => OOM_ERR,
    };
    encode_error(out, msg);
}

/// Encode an integer-or-error result as `:n\r\n` or the mapped error.
pub(crate) fn emit_int_result(res: Result<i64, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(n) => encode_integer(out, n),
        Err(e) => store_err(out, e),
    }
}

/// Encode a `Vec<Vec<u8>>` as a RESP array of bulk strings, or the mapped error.
pub(crate) fn emit_bulk_array(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(items) => {
            encode_array_len(out, items.len() as i64);
            for it in &items {
                encode_bulk(out, it);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `HSET key field value [field value ...]`.
pub(crate) fn cmd_hset<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return wrong_args(out, "hset");
    }
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (2..args.len())
        .step_by(2)
        .map(|i| (args[i].to_vec(), args[i + 1].to_vec()))
        .collect();
    emit_int_result(store.hset(&args[1], &pairs).map(|n| n as i64), out);
}

/// `ZADD key score member [score member ...]`.
pub(crate) fn cmd_zadd<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 || !(args.len() - 2).is_multiple_of(2) {
        return wrong_args(out, "zadd");
    }
    let mut pairs = Vec::with_capacity((args.len() - 2) / 2);
    let mut i = 2;
    while i < args.len() {
        let Some(score) = arg_f64(&args[i]) else {
            return encode_error(out, "ERR value is not a valid float");
        };
        pairs.push((score, args[i + 1].to_vec()));
        i += 2;
    }
    emit_int_result(store.zadd(&args[1], &pairs).map(|n| n as i64), out);
}

/// `ZRANGE key start stop [WITHSCORES]` — by rank.
pub(crate) fn cmd_zrange<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 4 || args.len() > 5 {
        return wrong_args(out, "zrange");
    }
    let withscores = args.len() == 5;
    if withscores && !args[4].eq_ignore_ascii_case(b"WITHSCORES") {
        return encode_error(out, "ERR syntax error");
    }
    let (Some(s), Some(e)) = (arg_i64(&args[2]), arg_i64(&args[3])) else {
        return encode_error(out, ERR_NOT_INT);
    };
    emit_zrange(store.zrange(&args[1], s, e), withscores, proto, out);
}

/// `ZRANGEBYSCORE key min max [WITHSCORES]`.
pub(crate) fn cmd_zrangebyscore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 4 || args.len() > 5 {
        return wrong_args(out, "zrangebyscore");
    }
    let withscores = args.len() == 5;
    if withscores && !args[4].eq_ignore_ascii_case(b"WITHSCORES") {
        return encode_error(out, "ERR syntax error");
    }
    let (Some(min), Some(max)) = (parse_score_bound(&args[2]), parse_score_bound(&args[3])) else {
        return encode_error(out, "ERR min or max is not a float");
    };
    emit_zrange(store.zrange_by_score(&args[1], min, max), withscores, proto, out);
}

/// Encode a `(member, score)` list per `withscores` + `proto`:
///
/// | mode                  | wire shape                                                          |
/// |-----------------------|---------------------------------------------------------------------|
/// | no WITHSCORES (both)  | `*N\r\n$<m>...` — flat array of bulks                              |
/// | WITHSCORES + V2       | `*2N\r\n$<m>\r\n$<s>...` — interleaved bulks (Redis legacy)        |
/// | WITHSCORES + V3       | `*N\r\n*2\r\n$<m>\r\n,<s>\r\n...` — array of [bulk, double] pairs  |
///
/// The V3 nested-array shape is what RESP3 clients expect; the V2 flat
/// interleaving is preserved bit-for-bit so unmigrated clients stay
/// happy.
pub(crate) fn emit_zrange(
    res: Result<Vec<(Vec<u8>, f64)>, StoreError>,
    withscores: bool,
    proto: RespVersion,
    out: &mut Vec<u8>,
) {
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => match (withscores, proto) {
            (false, _) => {
                encode_array_len(out, items.len() as i64);
                for (m, _) in &items {
                    encode_bulk(out, m);
                }
            }
            (true, RespVersion::V2) => {
                encode_array_len(out, (items.len() * 2) as i64);
                for (m, sc) in &items {
                    encode_bulk(out, m);
                    encode_bulk(out, &fmt_score(*sc));
                }
            }
            (true, RespVersion::V3) => {
                encode_array_len(out, items.len() as i64);
                for (m, sc) in &items {
                    encode_array_len(out, 2);
                    encode_bulk(out, m);
                    encode_double(out, *sc);
                }
            }
        },
    }
}

/// Parse an f64 score argument (accepts `inf`/`-inf`); rejects NaN.
pub(crate) fn arg_f64(b: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(b).ok()?.trim();
    let f: f64 = match s.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        _ => s.parse().ok()?,
    };
    if f.is_nan() { None } else { Some(f) }
}

/// Parse a `ZRANGEBYSCORE`/`ZCOUNT` bound: a leading `(` means exclusive.
pub(crate) fn parse_score_bound(b: &[u8]) -> Option<ScoreBound> {
    match b.strip_prefix(b"(") {
        Some(rest) => Some(ScoreBound {
            value: arg_f64(rest)?,
            exclusive: true,
        }),
        None => Some(ScoreBound {
            value: arg_f64(b)?,
            exclusive: false,
        }),
    }
}

/// Format a score the way Redis does: integral values without a decimal point.
pub(crate) fn fmt_score(s: f64) -> Vec<u8> {
    if s.is_infinite() {
        return if s > 0.0 {
            b"inf".to_vec()
        } else {
            b"-inf".to_vec()
        };
    }
    if s == s.trunc() && s.abs() < 1e17 {
        return (s as i64).to_string().into_bytes();
    }
    format!("{s}").into_bytes()
}


/// Owned copy of `args[from..]` as a `Vec<Vec<u8>>`, for the variadic
/// (multi-value) store calls that take `&[Vec<u8>]`. The headline single-key
/// commands don't use this; these multi-value commands hand the bytes to the
/// store to keep anyway.
pub(crate) fn rest<A: ArgvView + ?Sized>(args: &A, from: usize) -> Vec<Vec<u8>> {
    (from..args.len()).map(|i| args[i].to_vec()).collect()
}

/// Parse an `i64` argument from raw bytes.
pub(crate) fn arg_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse::<i64>().ok()
}

/// Extract the `MATCH <pattern>` option from a `SCAN cursor [opts...]` command.
pub(crate) fn scan_pattern<A: ArgvView + ?Sized>(args: &A) -> Option<Vec<u8>> {
    let mut i = 2;
    while i + 1 < args.len() {
        if args[i].eq_ignore_ascii_case(b"MATCH") {
            return Some(args[i + 1].to_vec());
        }
        i += 2;
    }
    None
}

// `cmd_set` / `cmd_setex` / `cmd_incr` / `cmd_incr_by` / `cmd_expire` /
// `cmd_ttl` / `cmd_pop` / `cmd_spop_rand` live in [`crate::cmd_data`];
// re-export them here so `use crate::cmd::*` in the dispatchers continues
// to find them.
pub(crate) use crate::cmd_data::*;
