//! Full-surface argv → RESP dispatcher — the engine entry every
//! language binding (kevy-ffi and its shells) reaches the embedded
//! store through: one function, the whole verb surface.
//!
//! Coverage = the ESTORE_OPS manifest (reads AND writes) plus the
//! conn-face verbs the in-process engine can honestly serve (PING /
//! ECHO / PUBLISH) — enforced both ways by `dispatch_tests.rs`
//! (every manifest verb has an arm; the arm table is a superset).
//! Reply bytes and error wording mirror the real server; the oracle
//! test in `tests/dispatch_oracle.rs` replays one deterministic
//! command sequence against `target/debug/kevy` and this dispatcher
//! and compares the wire bytes.
//!
//! The read-only listener whitelist (`listener/verbs.rs`) is a
//! separate, intentionally narrower surface and stays untouched.

mod bitmap;
#[cfg(feature = "index")]
mod describe;
mod hash;
#[cfg(feature = "index")]
mod idx;
#[cfg(feature = "index")]
mod idx_compose;
#[cfg(feature = "index")]
mod idx_create;
#[cfg(feature = "index")]
mod idx_query;
mod keyspace;
mod list;
mod misc;
mod set;
mod strings;
#[cfg(feature = "index")]
mod table;
#[cfg(feature = "index")]
mod view;
mod zset;
mod zset_algebra;

use kevy_resp::{encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk};
use kevy_verbs::reply::store_err_msg;

use crate::store::Store;
use crate::{KevyError, KevyResult};

/// Dispatch one command, appending the RESP-encoded reply to `out`.
pub(crate) fn dispatch(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(verb) = argv.first() else {
        return encode_error(out, "ERR empty command");
    };
    // every language binding funnels through here, so no heap allocation
    // for the verb; an over-long token misses every arm and is unknown
    let mut vbuf = [0u8; 32];
    let up = kevy_verbs::args::upper_verb(verb, &mut vbuf);
    let handled = strings::dispatch(s, up, argv, out)
        || hash::dispatch(s, up, argv, out)
        || list::dispatch(s, up, argv, out)
        || set::dispatch(s, up, argv, out)
        || zset::dispatch(s, up, argv, out)
        || zset_algebra::dispatch(s, up, argv, out)
        || bitmap::dispatch(s, up, argv, out)
        || keyspace::dispatch(s, up, argv, out)
        || misc::dispatch(s, up, argv, out)
        || dispatch_index(s, up, argv, out);
    if !handled {
        let shown = String::from_utf8_lossy(verb);
        encode_error(out, &format!("ERR unknown command '{shown}'"));
    }
}

#[cfg(feature = "index")]
fn dispatch_index(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    idx::dispatch(s, up, argv, out)
        || idx_query::dispatch(s, up, argv, out)
        || view::dispatch(s, up, argv, out)
        || table::dispatch(s, up, argv, out)
        || describe::dispatch(s, up, argv, out)
}

#[cfg(not(feature = "index"))]
fn dispatch_index(_s: &Store, _up: &[u8], _argv: &[Vec<u8>], _out: &mut Vec<u8>) -> bool {
    false
}

/// A facade error with the server's wording; the replica guard keeps
/// its bare `READONLY` prefix, as Redis does.
fn kevy_err(out: &mut Vec<u8>, e: &KevyError) {
    let msg: String = match e {
        KevyError::Store(se) => return encode_error(out, store_err_msg(se)),
        KevyError::ReadOnly => {
            return encode_error(out, "READONLY You can't write against a read only replica");
        }
        KevyError::InvalidInput(m) | KevyError::NotFound(m) | KevyError::Unsupported(m) => {
            format!("ERR {m}")
        }
        KevyError::Io(ioe) => {
            // catalog errors ride io::Error with an already-prefixed message
            let m = ioe.to_string();
            if m.starts_with("ERR ") { m } else { format!("ERR {m}") }
        }
        other => format!("ERR {other}"),
    };
    encode_error(out, &msg);
}

fn emit_int(out: &mut Vec<u8>, res: KevyResult<i64>) {
    match res {
        Ok(n) => encode_integer(out, n),
        Err(e) => kevy_err(out, &e),
    }
}

fn emit_bulk_array(out: &mut Vec<u8>, res: KevyResult<Vec<Vec<u8>>>) {
    match res {
        Ok(items) => {
            encode_array_len(out, items.len() as i64);
            for it in &items {
                encode_bulk(out, it);
            }
        }
        Err(e) => kevy_err(out, &e),
    }
}

fn opt_bulk(out: &mut Vec<u8>, v: Option<Vec<u8>>) {
    match v {
        Some(b) => encode_bulk(out, &b),
        None => encode_null_bulk(out),
    }
}

/// The verb as Redis names it in an error: argv[0], lowercased.
fn verb_name(argv: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(argv.first().map(Vec::as_slice).unwrap_or(b"")).to_lowercase()
}

fn rest(argv: &[Vec<u8>], from: usize) -> Vec<&[u8]> {
    argv[from..].iter().map(Vec::as_slice).collect()
}

// ---- helpers shared by the scan-shaped verbs ---------------------------

/// `[MATCH pattern] [COUNT n]` modifiers from `start` on. COUNT is
/// validated then ignored (one-batch scans, the server's shape).
/// `None` = syntax error.
fn parse_match_count(argv: &[Vec<u8>], start: usize) -> Option<Option<Vec<u8>>> {
    let mut pat: Option<Vec<u8>> = None;
    let mut i = start;
    while i < argv.len() {
        let tok = &argv[i];
        if tok.eq_ignore_ascii_case(b"MATCH") {
            pat = Some(argv.get(i + 1)?.clone());
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"COUNT") {
            kevy_verbs::args::arg_i64(argv.get(i + 1)?)?;
            i += 2;
        } else {
            return None;
        }
    }
    Some(pat)
}

/// `[cursor, [elems…]]` — the H/Z/S-SCAN reply envelope.
fn emit_scan_page(out: &mut Vec<u8>, cursor: &[u8], elems: &[Vec<u8>]) {
    encode_array_len(out, 2);
    encode_bulk(out, cursor);
    encode_array_len(out, elems.len() as i64);
    for e in elems {
        encode_bulk(out, e);
    }
}

/// Every verb the dispatcher owns an arm for — the parity tests hold
/// this table against `op_manifest::ESTORE_OPS` (⊇) and probe each
/// name for a real arm.
#[cfg(test)]
// LOC-WAIVER: pure data table — one row per dispatched verb.
pub(crate) const DISPATCH_VERBS: &[&str] = &[
    // strings
    "APPEND",
    "DECR",
    "DECRBY",
    "GET",
    "GETDEL",
    "GETEX",
    "GETRANGE",
    "GETSET",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "MGET",
    "MSET",
    "SET",
    "SETNX",
    "SETRANGE",
    "STRLEN",
    // bitmap
    "BITCOUNT",
    "BITOP",
    "BITPOS",
    "GETBIT",
    "SETBIT",
    // hashes
    "HDEL",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HRANDFIELD",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HSCAN",
    "HSET",
    "HSETNX",
    "HEXPIRE",
    "HPEXPIRE",
    "HPEXPIREAT",
    "HTTL",
    "HPTTL",
    "HPERSIST",
    "HVALS",
    // lists
    "LINDEX",
    "LINSERT",
    "LLEN",
    "LPOP",
    "LPUSH",
    "LRANGE",
    "LREM",
    "LSET",
    "LTRIM",
    "RPOP",
    "RPUSH",
    // sets
    "SADD",
    "SCARD",
    "SDIFF",
    "SDIFFSTORE",
    "SINTER",
    "SINTERSTORE",
    "SISMEMBER",
    "SMEMBERS",
    "SPOP",
    "SRANDMEMBER",
    "SREM",
    "SUNION",
    "SUNIONSTORE",
    // zsets
    "ZADD",
    "ZCARD",
    "ZCOUNT",
    "ZDIFFSTORE",
    "ZINCRBY",
    "ZINTERCARD",
    "ZINTERSTORE",
    "ZPOPMIN",
    "ZPOPMIN.BELOW",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREM",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZSCAN",
    "ZSCORE",
    "ZUNIONSTORE",
    // keyspace
    "COPY",
    "DBSIZE",
    "DEL",
    "EXISTS",
    "EXPIRE",
    "EXPIREAT",
    "FLUSHALL",
    "KEYS",
    "PERSIST",
    "PEXPIRE",
    "PEXPIREAT",
    "PTTL",
    "RANDOMKEY",
    "RENAME",
    "RENAMENX",
    "SCAN",
    "TIME",
    "TOUCH",
    "TTL",
    "TYPE",
    "UNLINK",
    // feed + digests
    "FEED.READ",
    "FEED.SHARDS",
    "FEED.TAIL",
    "PREFIX.DIGEST",
    "PREFIX.STATS",
    // index + views + tables
    "IDX.ADVISE",
    "IDX.COUNT",
    "IDX.CREATE",
    "IDX.DESCRIBE",
    "IDX.DROP",
    "IDX.LIST",
    "IDX.QUERY",
    "VIEW.CREATE",
    "VIEW.DESCRIBE",
    "VIEW.DROP",
    "VIEW.LIST",
    "VIEW.QUERY",
    "TABLE.DECLARE",
    "TABLE.ENSURE",
    "TABLE.REPLACE",
    "TABLE.DROP",
    "TABLE.LIST",
    "TABLE.VERIFY",
    "TABLE.DESCRIBE",
    // conn face
    "ECHO",
    "PING",
    "PUBLISH",
];

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
