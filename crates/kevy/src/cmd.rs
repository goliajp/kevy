//! Command helpers shared by the dispatcher.

use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_double, encode_error,
    encode_integer,
};
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
    encode_error(out, &format!("ERR wrong number of arguments for '{cmd}' command"));
}

/// Nothing in the local dispatch chain handled this verb — say which of
/// the two reasons that is.
///
/// A verb the metadata table knows is not an unknown command: it is a known
/// one called with the wrong count, which the extension routes in
/// `cmd_resolve` guard (`IDX.QUERY if args.len() >= 4`) and let fall
/// through to here when the guard misses. Twelve of fourteen guarded verbs
/// answered a wrong-arity call with "unknown command 'IDX.QUERY'" —
/// telling the caller a command that exists does not — until this site
/// started consulting the arity `verb_meta` had all along.
///
/// Redis's convention, and this engine's own everywhere else: negative
/// arity is a minimum, positive is exact.
pub(crate) fn unhandled_verb(out: &mut Vec<u8>, name: &[u8], nargs: usize) {
    let mut buf = [0u8; 32];
    let upper = upper_verb(name, &mut buf);
    if let Some(meta) = std::str::from_utf8(upper).ok().and_then(crate::verb_meta::verb_meta) {
        let (n, a) = (nargs as i64, i64::from(meta.arity));
        if (a < 0 && n < -a) || (a > 0 && n != a) {
            return wrong_args(out, &meta.name.to_lowercase());
        }
    }
    let shown = String::from_utf8_lossy(name);
    encode_error(out, &format!("ERR unknown command '{shown}'"));
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
pub(crate) const OOM_ERR: &str = "OOM command not allowed when used memory > 'maxmemory'.";

/// Verb classification tables (`is_write_verb` / `notify_class_for_verb` /
/// `is_growing_write_verb`) live in [`crate::cmd_class`]; re-exported here
/// so dispatchers keep their `cmd::*` paths.
pub(crate) use crate::cmd_class::{is_growing_write_verb, is_write_verb, notify_class_for_verb};

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

/// `HSET key field value [field value ...]`. Borrowed-pair path: the pair
/// list holds `&[u8]` slices into argv, avoiding a `Vec<u8>` alloc per
/// field+value.
pub(crate) fn cmd_hset<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return wrong_args(out, "hset");
    }
    let pairs: Vec<(&[u8], &[u8])> =
        (2..args.len()).step_by(2).map(|i| (&args[i], &args[i + 1])).collect();
    emit_int_result(store.hset(&args[1], &pairs).map(|n| n as i64), out);
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

/// `ZREVRANGE key start stop [WITHSCORES]` — the by-rank range read
/// from the high end.
///
/// `Store::zrevrange` does the work — one implementation, shared with
/// the embedded facade. Each surface had written its own before, and
/// both had written the same bug: a positive start clamped up to the
/// last rank, so `ZREVRANGE z 5 10` on a three-member set answered a
/// member where Redis answers none. They agreed with each other, which
/// is why the wire-vs-facade differential passed it and the three-way
/// against a real valkey did not.
pub(crate) fn cmd_zrevrange<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 4 || args.len() > 5 {
        return wrong_args(out, "zrevrange");
    }
    let withscores = args.len() == 5;
    if withscores && !args[4].eq_ignore_ascii_case(b"WITHSCORES") {
        return encode_error(out, "ERR syntax error");
    }
    let (Some(start), Some(stop)) = (arg_i64(&args[2]), arg_i64(&args[3])) else {
        return encode_error(out, ERR_NOT_INT);
    };
    emit_zrange(store.zrevrange(&args[1], start, stop), withscores, proto, out);
}

/// `ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]`.
///
/// BullMQ uses `LIMIT 0 1` inside its `moveToActive` /
/// `addJob` scripts; the modifier may appear in either order
/// relative to `WITHSCORES`. We accept either order to match Redis.
pub(crate) fn cmd_zrangebyscore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    if args.len() < 4 {
        return wrong_args(out, "zrangebyscore");
    }
    let (Some(min), Some(max)) = (parse_score_bound(&args[2]), parse_score_bound(&args[3])) else {
        return encode_error(out, "ERR min or max is not a float");
    };
    let Some((withscores, limit)) = parse_zrbs_modifiers(args, out) else {
        return; // error already encoded
    };
    let res = store.zrange_by_score(&args[1], min, max);
    match res {
        Err(e) => store_err(out, e),
        Ok(mut items) => {
            if let Some((off, cnt)) = limit {
                let start = off.max(0) as usize;
                if start >= items.len() {
                    items.clear();
                } else if cnt < 0 {
                    // Redis: negative count = all remaining.
                    items.drain(..start);
                } else {
                    let end = (start + cnt as usize).min(items.len());
                    items = items[start..end].to_vec();
                }
            }
            emit_zrange(Ok(items), withscores, proto, out);
        }
    }
}

/// Parse the optional `ZRANGEBYSCORE` modifiers — `WITHSCORES` and
/// `LIMIT offset count` can appear in either order, no more than once
/// each. `None` = a syntax error was already encoded into `out`.
fn parse_zrbs_modifiers<A: ArgvView + ?Sized>(
    args: &A,
    out: &mut Vec<u8>,
) -> Option<(bool, Option<(i64, i64)>)> {
    let mut withscores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"WITHSCORES") {
            if withscores {
                encode_error(out, "ERR syntax error");
                return None;
            }
            withscores = true;
            i += 1;
        } else if tok.eq_ignore_ascii_case(b"LIMIT") {
            if limit.is_some() || i + 2 >= args.len() {
                encode_error(out, "ERR syntax error");
                return None;
            }
            let Some(off) =
                std::str::from_utf8(&args[i + 1]).ok().and_then(|s| s.parse::<i64>().ok())
            else {
                encode_error(out, ERR_NOT_INT);
                return None;
            };
            let Some(cnt) =
                std::str::from_utf8(&args[i + 2]).ok().and_then(|s| s.parse::<i64>().ok())
            else {
                encode_error(out, ERR_NOT_INT);
                return None;
            };
            limit = Some((off, cnt));
            i += 3;
        } else {
            encode_error(out, "ERR syntax error");
            return None;
        }
    }
    Some((withscores, limit))
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
        Some(rest) => Some(ScoreBound { value: arg_f64(rest)?, exclusive: true }),
        None => Some(ScoreBound { value: arg_f64(b)?, exclusive: false }),
    }
}

/// Format a score the way Redis does: integral values without a decimal point.
pub(crate) fn fmt_score(s: f64) -> Vec<u8> {
    if s.is_infinite() {
        return if s > 0.0 { b"inf".to_vec() } else { b"-inf".to_vec() };
    }
    // Bit-exact integer-valued check; epsilon would change the wire shape.
    #[allow(clippy::float_cmp)]
    let is_integer_valued = s == s.trunc();
    if is_integer_valued && s.abs() < 1e17 {
        return (s as i64).to_string().into_bytes();
    }
    format!("{s}").into_bytes()
}

/// Borrowed `args[from..]` as `Vec<&[u8]>` — zero per-member heap
/// alloc. Mirrors valkey's `c->argv[j]`-without-copy hand-off (`t_set.c:611`
/// `setTypeAdd(set, objectGetVal(c->argv[j]))`). Paired with the Store
/// `*_borrowed` family that takes `&[&[u8]]`; the Store then materialises
/// `SmallBytes` per member once at insert (same as before), but the dispatch
/// hand-off no longer pays a `Vec<u8>` per arg.
pub(crate) fn rest_borrowed<A: ArgvView + ?Sized>(args: &A, from: usize) -> Vec<&[u8]> {
    (from..args.len()).map(|i| &args[i]).collect()
}

/// Parse an `i64` argument from raw bytes.
pub(crate) fn arg_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse::<i64>().ok()
}

/// Parse `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]` into
/// the runtime's [`kevy_rt::ScanArgs`]. `Err` carries the exact error
/// message the runtime puts on the wire (Redis wording).
pub(crate) fn scan_args<A: ArgvView + ?Sized>(args: &A) -> Result<kevy_rt::ScanArgs, &'static str> {
    let cursor: u64 = std::str::from_utf8(&args[1])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("ERR invalid cursor")?;
    let mut count = 10usize; // Redis default work bound
    let mut pattern = None;
    let mut type_filter = None;
    let mut i = 2;
    while i < args.len() {
        let opt = &args[i];
        let Some(val) = args.get(i + 1) else {
            return Err("ERR syntax error");
        };
        if opt.eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(val.to_vec());
        } else if opt.eq_ignore_ascii_case(b"COUNT") {
            let n: i64 = std::str::from_utf8(val)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or("ERR value is not an integer or out of range")?;
            if n < 1 {
                return Err("ERR syntax error");
            }
            count = n as usize;
        } else if opt.eq_ignore_ascii_case(b"TYPE") {
            type_filter = Some(val.to_vec());
        } else {
            return Err("ERR syntax error");
        }
        i += 2;
    }
    Ok(kevy_rt::ScanArgs { cursor, count, pattern, type_filter })
}

// `cmd_set` / `cmd_setex` / `cmd_incr` / `cmd_incr_by` / `cmd_expire` /
// `cmd_ttl` / `cmd_pop` / `cmd_spop_rand` live in [`crate::cmd_data`];
// re-export them here so `use crate::cmd::*` in the dispatchers continues
// to find them.
pub(crate) use crate::cmd_data::*;
