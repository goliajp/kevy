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
mod util;
#[cfg(feature = "index")]
mod view;
mod zset;
mod zset_algebra;

use crate::store::Store;

/// Stack-buffer width for the uppercased verb. 16 bytes covers every real
/// verb (the longest in [`DISPATCH_VERBS`] is `ZREMRANGEBYSCORE` at 16), so
/// the common path never touches the heap.
const UPPER_STACK: usize = 16;

/// Dispatch one command, appending the RESP-encoded reply to `out`.
pub(crate) fn dispatch(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(verb) = argv.first() else {
        return util::err(out, "ERR empty command");
    };
    // Uppercase the verb into a stack buffer — no per-command heap alloc on
    // this universal FFI entry (every language binding funnels through here).
    // A pathologically long first token (> UPPER_STACK) falls back to the heap
    // path so behaviour is identical: it just misses every arm and answers
    // "unknown command", exactly as the old `to_ascii_uppercase` Vec did.
    let mut vbuf = [0u8; UPPER_STACK];
    let vheap;
    let up: &[u8] = if verb.len() <= UPPER_STACK {
        for (slot, &b) in vbuf.iter_mut().zip(verb.iter()) {
            *slot = b.to_ascii_uppercase();
        }
        &vbuf[..verb.len()]
    } else {
        vheap = verb.to_ascii_uppercase();
        &vheap
    };
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
        util::err(out, &format!("ERR unknown command '{shown}'"));
    }
}

#[cfg(feature = "index")]
fn dispatch_index(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    idx::dispatch(s, up, argv, out)
        || idx_query::dispatch(s, up, argv, out)
        || view::dispatch(s, up, argv, out)
        || table::dispatch(s, up, argv, out)
}

#[cfg(not(feature = "index"))]
fn dispatch_index(_s: &Store, _up: &[u8], _argv: &[Vec<u8>], _out: &mut Vec<u8>) -> bool {
    false
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
            util::arg_i64(argv.get(i + 1)?)?;
            i += 2;
        } else {
            return None;
        }
    }
    Some(pat)
}

/// `[cursor, [elems…]]` — the H/Z/S-SCAN reply envelope.
fn emit_scan_page(out: &mut Vec<u8>, cursor: &[u8], elems: &[Vec<u8>]) {
    util::arr(out, 2);
    util::bulk(out, cursor);
    util::arr(out, elems.len());
    for e in elems {
        util::bulk(out, e);
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
    "IDX.DROP",
    "IDX.LIST",
    "IDX.QUERY",
    "VIEW.CREATE",
    "VIEW.DROP",
    "VIEW.LIST",
    "VIEW.QUERY",
    "TABLE.DECLARE",
    "TABLE.ENSURE",
    "TABLE.REPLACE",
    "TABLE.DROP",
    "TABLE.LIST",
    "TABLE.VERIFY",
    // conn face
    "ECHO",
    "PING",
    "PUBLISH",
];

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
