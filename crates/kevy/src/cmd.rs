//! Command helpers shared by the dispatcher.

use kevy_resp::{
    encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
use kevy_store::{ScoreBound, Store, StoreError};
use std::time::Duration;

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

/// Encode a `StoreError` as its RESP error reply.
pub(crate) fn store_err(out: &mut Vec<u8>, e: StoreError) {
    let msg = match e {
        StoreError::WrongType => WRONGTYPE,
        StoreError::NotInteger => ERR_NOT_INT,
        StoreError::Overflow => "ERR increment or decrement would overflow",
        StoreError::OutOfRange => "ERR index out of range",
        StoreError::NoSuchKey => "ERR no such key",
        StoreError::NotFloat => "ERR value is not a valid float",
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
pub(crate) fn cmd_hset(store: &mut Store, args: &[Vec<u8>], out: &mut Vec<u8>) {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return wrong_args(out, "hset");
    }
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = args[2..]
        .chunks(2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect();
    emit_int_result(store.hset(&args[1], &pairs).map(|n| n as i64), out);
}

/// `ZADD key score member [score member ...]`.
pub(crate) fn cmd_zadd(store: &mut Store, args: &[Vec<u8>], out: &mut Vec<u8>) {
    if args.len() < 4 || !(args.len() - 2).is_multiple_of(2) {
        return wrong_args(out, "zadd");
    }
    let mut pairs = Vec::with_capacity((args.len() - 2) / 2);
    let mut i = 2;
    while i < args.len() {
        let Some(score) = arg_f64(&args[i]) else {
            return encode_error(out, "ERR value is not a valid float");
        };
        pairs.push((score, args[i + 1].clone()));
        i += 2;
    }
    emit_int_result(store.zadd(&args[1], &pairs).map(|n| n as i64), out);
}

/// `ZRANGE key start stop [WITHSCORES]` — by rank.
pub(crate) fn cmd_zrange(store: &mut Store, args: &[Vec<u8>], out: &mut Vec<u8>) {
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
    emit_zrange(store.zrange(&args[1], s, e), withscores, out);
}

/// `ZRANGEBYSCORE key min max [WITHSCORES]`.
pub(crate) fn cmd_zrangebyscore(store: &mut Store, args: &[Vec<u8>], out: &mut Vec<u8>) {
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
    emit_zrange(store.zrange_by_score(&args[1], min, max), withscores, out);
}

/// Encode a `(member, score)` list as a RESP array, optionally `WITHSCORES`.
pub(crate) fn emit_zrange(
    res: Result<Vec<(Vec<u8>, f64)>, StoreError>,
    withscores: bool,
    out: &mut Vec<u8>,
) {
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            let n = if withscores {
                items.len() * 2
            } else {
                items.len()
            };
            encode_array_len(out, n as i64);
            for (m, sc) in &items {
                encode_bulk(out, m);
                if withscores {
                    encode_bulk(out, &fmt_score(*sc));
                }
            }
        }
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

/// `SPOP`/`SRANDMEMBER key [count]` — single reply without count, array with it.
pub(crate) fn cmd_spop_rand(store: &mut Store, args: &[Vec<u8>], remove: bool, out: &mut Vec<u8>) {
    let name = if remove { "spop" } else { "srandmember" };
    if args.len() < 2 || args.len() > 3 {
        return wrong_args(out, name);
    }
    let count_given = args.len() == 3;
    let count = if count_given {
        match arg_i64(&args[2]) {
            Some(c) if c >= 0 => c as usize,
            _ => return encode_error(out, "ERR value is out of range, must be positive"),
        }
    } else {
        1
    };
    let res = if remove {
        store.spop(&args[1], count)
    } else {
        store.srandmember(&args[1], count)
    };
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if count_given {
                encode_array_len(out, items.len() as i64);
                for it in &items {
                    encode_bulk(out, it);
                }
            } else {
                match items.first() {
                    Some(v) => encode_bulk(out, v),
                    None => encode_null_bulk(out),
                }
            }
        }
    }
}

/// `LPOP`/`RPOP key [count]` — single reply without count, array with it.
pub(crate) fn cmd_pop(store: &mut Store, args: &[Vec<u8>], tail: bool, out: &mut Vec<u8>) {
    let name = if tail { "rpop" } else { "lpop" };
    if args.len() < 2 || args.len() > 3 {
        return wrong_args(out, name);
    }
    let count_given = args.len() == 3;
    let count = if count_given {
        match arg_i64(&args[2]) {
            Some(c) if c >= 0 => c as usize,
            _ => return encode_error(out, "ERR value is out of range, must be positive"),
        }
    } else {
        1
    };
    let res = if tail {
        store.rpop(&args[1], count)
    } else {
        store.lpop(&args[1], count)
    };
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if count_given {
                if items.is_empty() {
                    out.extend_from_slice(b"*-1\r\n"); // nil array (key absent / empty)
                } else {
                    encode_array_len(out, items.len() as i64);
                    for it in &items {
                        encode_bulk(out, it);
                    }
                }
            } else {
                match items.first() {
                    Some(v) => encode_bulk(out, v),
                    None => encode_null_bulk(out),
                }
            }
        }
    }
}

/// `SET key value [EX s | PX ms] [NX | XX]`.
pub(crate) fn cmd_set(store: &mut Store, args: &[Vec<u8>], out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "set");
    }
    let mut expire: Option<Duration> = None;
    let mut nx = false;
    let mut xx = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].to_ascii_uppercase().as_slice() {
            b"NX" => nx = true,
            b"XX" => xx = true,
            opt @ (b"EX" | b"PX") => {
                let Some(raw) = args.get(i + 1) else {
                    return encode_error(out, "ERR syntax error");
                };
                let Some(n) = arg_i64(raw).filter(|&n| n > 0) else {
                    return encode_error(out, "ERR invalid expire time in 'set' command");
                };
                let ms = if opt == b"EX" {
                    n.saturating_mul(1000)
                } else {
                    n
                };
                expire = Some(Duration::from_millis(ms as u64));
                i += 1;
            }
            _ => return encode_error(out, "ERR syntax error"),
        }
        i += 1;
    }
    if nx && xx {
        return encode_error(out, "ERR syntax error");
    }
    if store.set(&args[1], args[2].clone(), expire, nx, xx) {
        encode_simple_string(out, "OK");
    } else {
        encode_null_bulk(out); // NX/XX condition not met
    }
}

/// `SETEX`/`PSETEX key ttl value`.
pub(crate) fn cmd_setex(
    store: &mut Store,
    args: &[Vec<u8>],
    unit_ms: i64,
    name: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 4 {
        return wrong_args(out, name);
    }
    let Some(n) = arg_i64(&args[2]).filter(|&n| n > 0) else {
        return encode_error(out, &format!("ERR invalid expire time in '{name}' command"));
    };
    let ms = n.saturating_mul(unit_ms) as u64;
    store.set(
        &args[1],
        args[3].clone(),
        Some(Duration::from_millis(ms)),
        false,
        false,
    );
    encode_simple_string(out, "OK");
}

pub(crate) fn cmd_incr(
    store: &mut Store,
    args: &[Vec<u8>],
    delta: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 2 {
        return wrong_args(out, cmd);
    }
    emit_int_result(store.incr_by(&args[1], delta), out);
}

pub(crate) fn cmd_incr_by(
    store: &mut Store,
    args: &[Vec<u8>],
    negate: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return wrong_args(out, cmd);
    }
    let Some(mut delta) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if negate {
        let Some(neg) = delta.checked_neg() else {
            return encode_error(out, "ERR decrement would overflow");
        };
        delta = neg;
    }
    emit_int_result(store.incr_by(&args[1], delta), out);
}

/// `EXPIRE`/`PEXPIRE`: non-positive TTL deletes the key (returning 1 if it
/// existed), matching Redis.
pub(crate) fn cmd_expire(
    store: &mut Store,
    args: &[Vec<u8>],
    unit_ms: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return wrong_args(out, cmd);
    }
    let Some(n) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if store.exists(&[args[1].clone()]) == 0 {
        return encode_integer(out, 0);
    }
    if n <= 0 {
        store.del(&[args[1].clone()]);
        return encode_integer(out, 1);
    }
    let ms = n.saturating_mul(unit_ms) as u64;
    encode_integer(
        out,
        store.expire(&args[1], Duration::from_millis(ms)) as i64,
    );
}

/// `TTL` (seconds) / `PTTL` (millis). Pass-through of the -2 / -1 sentinels.
pub(crate) fn cmd_ttl(
    store: &mut Store,
    args: &[Vec<u8>],
    in_secs: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 2 {
        return wrong_args(out, cmd);
    }
    let ms = store.pttl(&args[1]);
    let val = if in_secs && ms >= 0 {
        (ms + 500) / 1000
    } else {
        ms
    };
    encode_integer(out, val);
}

/// Parse an `i64` argument from raw bytes.
pub(crate) fn arg_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse::<i64>().ok()
}

/// Extract the `MATCH <pattern>` option from a `SCAN cursor [opts...]` command.
pub(crate) fn scan_pattern(args: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut i = 2;
    while i + 1 < args.len() {
        if args[i].eq_ignore_ascii_case(b"MATCH") {
            return Some(args[i + 1].clone());
        }
        i += 2;
    }
    None
}
