//! OP_TABLE — the single source of truth for kevy's command surface:
//! one registry, not per-surface drip-feed.
//!
//! Pure `const` data, **not** codegen: every dispatch match and facade
//! method stays hand-written. Each crate exports a manifest of the op
//! names its surfaces implement; CI parity tests cross-check every
//! manifest against this table and fail listing the missing
//! `(op, surface)` pairs. The five hand-maintained classification
//! lists in the server (`is_write_verb` / `is_growing_write_verb` /
//! `notify_class_for_verb` / wake list / Lua wake list) are grounded
//! by runtime tests iterating these rows.
//!
//! Empirical justification: an op-surface audit once found 10 facade
//! verbs missing from embedded replay — silent data loss on reopen
//! that shipped across many releases before being caught. This table
//! makes that drift class a CI failure.

/// Surface bits: where an op is implemented **today**. Absence of a
/// bit is ground truth, not aspiration — should-exist-but-doesn't
/// lives in [`KNOWN_GAPS`], which parity tests keep exhaustive.
pub mod surface {
    /// Server RESP dispatch (`kevy` crate).
    pub const SERVER: u16 = 1 << 0;
    /// Embedded `Store` facade method (`kevy-embedded`).
    pub const ESTORE: u16 = 1 << 1;
    /// Embedded `Pipeline` entry.
    pub const PIPE: u16 = 1 << 2;
    /// Embedded `AtomicCtx` **and** `AtomicAllShards` (the two must
    /// never drift — the parity test asserts both).
    pub const ATOMIC: u16 = 1 << 3;
    /// Embedded AOF replay arm (`replay.rs`) — REQUIRED for every
    /// verb any embedded facade logs, and for every server verb an
    /// embed-as-replica must apply.
    pub const REPLAY: u16 = 1 << 4;
    /// AOF rewrite emit set (`kevy-persist::rewrite_fmt`).
    pub const REWRITE: u16 = 1 << 5;
}

/// Keyspace-notification class (neutral mirror of `kevy-rt`'s
/// `NotifyClass`, which this crate cannot depend on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    /// Redis notification class `$` (string commands).
    String,
    /// Class `h`.
    Hash,
    /// Class `l`.
    List,
    /// Class `s`.
    Set,
    /// Class `z`.
    Zset,
    /// Class `t`.
    Stream,
    /// Class `g` (DEL / EXPIRE / PERSIST …).
    Generic,
}

/// One registry row: a command's classification + the surfaces it
/// exists on today.
#[derive(Debug, Clone, Copy)]
pub struct OpSpec {
    /// Canonical uppercase command name.
    pub name: &'static str,
    /// Server `is_write_verb` classification (AOF/replication gate).
    pub write: bool,
    /// Subset of `write` that can grow memory (OOM precheck).
    pub growing: bool,
    /// Keyspace-notification class; `None` = no notification.
    pub notify: Option<NotifyKind>,
    /// Producer verbs that wake blocked waiters: key arg index.
    pub wake_idx: Option<u8>,
    /// Bitset of [`surface`] flags where the op exists today.
    pub surfaces: u16,
}

const fn op(
    name: &'static str,
    write: bool,
    growing: bool,
    notify: Option<NotifyKind>,
    wake_idx: Option<u8>,
    surfaces: u16,
) -> OpSpec {
    OpSpec { name, write, growing, notify, wake_idx, surfaces }
}

use NotifyKind as N;
use surface::{ATOMIC, ESTORE, PIPE, REPLAY, REWRITE, SERVER};

const RD: bool = false; // read
const WR: bool = true; // write
const GROW: bool = true;
const NG: bool = false; // non-growing

/// The registry. One row per command. Kept grouped by type family and
/// alphabetical inside each group so a missing row is easy to spot.
#[rustfmt::skip]
pub const OP_TABLE: &[OpSpec] = &[
    // ---- strings -----------------------------------------------------
    op("APPEND",       WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("DECR",         WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("DECRBY",       WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("GET",          RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("GETDEL",       WR, NG,   Some(N::String), None,    SERVER | ESTORE | REPLAY),
    // GETEX's notify column is None on purpose. Redis fires `expire`
    // (class Generic) for the EX/PX form and nothing for the bare one;
    // it never emits a `getex` event. This engine keys the event NAME
    // off the verb, so any class here would publish a name Redis does
    // not have. The column stayed Some(String) while the verb was
    // ESTORE-only and nothing on the server could act on it.
    op("GETEX",        WR, NG,   None,            None,    SERVER | ESTORE),
    op("GETRANGE",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("GETSET",       WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("INCR",         WR, GROW, Some(N::String), None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("INCRBY",       WR, GROW, Some(N::String), None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("INCRBYFLOAT",  WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("MGET",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("MSET",         WR, GROW, None,            None,    SERVER | ESTORE),
    op("PSETEX",       WR, GROW, Some(N::String), None,    SERVER),
    op("SET",          WR, GROW, Some(N::String), None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    op("SETEX",        WR, GROW, Some(N::String), None,    SERVER),
    op("SETNX",        WR, GROW, Some(N::String), None,    SERVER | ESTORE),
    op("SETRANGE",     WR, GROW, Some(N::String),            None,    SERVER | ESTORE | REPLAY),
    op("STRLEN",       RD, NG,   None,            None,    SERVER | ESTORE),
    // ---- bitmap (string-backed) ---------------------------------------
    op("BITCOUNT",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("BITOP",        WR, GROW, None,            None,    ESTORE),
    op("BITPOS",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("GETBIT",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("SETBIT",       WR, GROW, Some(N::String),            None,    SERVER | ESTORE | REPLAY),
    // ---- hashes -------------------------------------------------------
    op("HDEL",         WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("HEXISTS",      RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("HGET",         RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("HGETALL",      RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("HINCRBY",      WR, GROW, Some(N::Hash),   None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("HINCRBYFLOAT", WR, GROW, Some(N::Hash),            None,    SERVER | ESTORE | REPLAY),
    op("HKEYS",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("HLEN",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("HMGET",        RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("HMSET",        WR, GROW, Some(N::Hash),   None,    SERVER),
    op("HSCAN",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("HSET",         WR, GROW, Some(N::Hash),   None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    // Hash field TTLs (Redis 7.4). Relative forms are
    // effect-logged as the absolute HPEXPIREAT (exemption below);
    // HPEXPIREAT is the canonical replay/rewrite carrier.
    op("HEXPIRE",      WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE),
    op("HPEXPIRE",     WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE),
    op("HPEXPIREAT",   WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | REPLAY | REWRITE),
    op("HTTL",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("HPTTL",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("HPERSIST",     WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | REPLAY),
    op("HSETNX",       WR, GROW, Some(N::Hash),   None,    SERVER | ESTORE | REPLAY),
    op("HVALS",        RD, NG,   None,            None,    SERVER | ESTORE),
    // ---- lists --------------------------------------------------------
    // BLPOP/BRPOP never write directly: the blocked-serve path
    // executes (and AOF-logs) the effect as a plain LPOP/RPOP.
    op("BLPOP",        RD, NG,   None,            None,    SERVER),
    op("BRPOP",        RD, NG,   None,            None,    SERVER),
    // Blocking form notifies via its executed effect, not the verb.
    op("BRPOPLPUSH",   WR, GROW, None,            None,    SERVER),
    op("LINDEX",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("LINSERT",      WR, GROW, Some(N::List),            None,    SERVER | ESTORE | REPLAY),
    op("LLEN",         RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("LMOVE",        WR, GROW, Some(N::List),   None,    SERVER),
    op("LPOP",         WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("LPOS",         RD, NG,   None,            None,    SERVER),
    op("LPUSH",        WR, GROW, Some(N::List),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("LRANGE",       RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("LREM",         WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("LSET",         WR, GROW, Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("LTRIM",        WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("RPOP",         WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("RPOPLPUSH",    WR, GROW, Some(N::List),   None,    SERVER),
    op("RPUSH",        WR, GROW, Some(N::List),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    // ---- sets ---------------------------------------------------------
    op("SADD",         WR, GROW, Some(N::Set),    None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    op("SCARD",        RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("SDIFF",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("SINTER",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("SINTERSTORE",  WR, GROW, Some(N::Set),    None,    SERVER | ESTORE),
    op("SISMEMBER",    RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("SMEMBERS",     RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("SPOP",         WR, NG,   Some(N::Set),    None,    SERVER | ESTORE | REPLAY),
    op("SRANDMEMBER",  RD, NG,   None,            None,    SERVER | ESTORE),
    op("SREM",         WR, NG,   Some(N::Set),    None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("SSCAN",        RD, NG,   None,            None,    SERVER),
    op("SUNION",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("SUNIONSTORE",  WR, GROW, Some(N::Set),    None,    SERVER | ESTORE),
    op("SDIFFSTORE",   WR, GROW, Some(N::Set),    None,    SERVER | ESTORE),
    // ---- zsets --------------------------------------------------------
    op("BZPOPMIN",     WR, NG,   None,            None,    SERVER),
    op("ZADD",         WR, GROW, Some(N::Zset),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    op("ZCARD",        RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("ZCOUNT",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZINCRBY",      WR, GROW, Some(N::Zset),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    // Set/zset algebra stores: effect-logged as DEL+ZADD/SADD, so no
    // REPLAY arm of their own is needed (the effect verbs replay).
    op("ZINTERSTORE",  WR, GROW, Some(N::Zset),   None,    SERVER | ESTORE),
    // Delayed-job primitive; embedded logs the ZREM effect.
    op("ZPOPMIN.BELOW", WR, NG,  Some(N::Zset),   None,    SERVER | ESTORE),
    op("ZUNIONSTORE",  WR, GROW, Some(N::Zset),   None,    SERVER | ESTORE),
    op("ZDIFFSTORE",   WR, GROW, Some(N::Zset),   None,    SERVER | ESTORE),
    op("ZINTERCARD",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZPOPMIN",      WR, NG,   Some(N::Zset),   None,    SERVER | ESTORE | REPLAY),
    op("ZRANGE",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZRANGEBYSCORE", RD, NG,  None,            None,    SERVER | ESTORE | ATOMIC),
    op("ZRANK",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZREM",         WR, NG,   Some(N::Zset),   None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("ZREMRANGEBYRANK",  WR, NG, Some(N::Zset), None,    SERVER | ESTORE | REPLAY),
    op("ZREMRANGEBYSCORE", WR, NG, Some(N::Zset), None,    SERVER | ESTORE | REPLAY),
    op("ZREVRANGE",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZREVRANGEBYSCORE", RD, NG, None,          None,    SERVER | ESTORE),
    op("ZSCAN",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZSCORE",       RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    // ---- streams (server-only today; embedded exposure undecided) -----
    op("XACK",         WR, NG,   Some(N::Stream), None,    SERVER),
    op("XADD",         WR, GROW, Some(N::Stream), Some(1), SERVER | REWRITE),
    op("XAUTOCLAIM",   WR, GROW, Some(N::Stream), None,    SERVER),
    op("XCLAIM",       WR, GROW, Some(N::Stream), None,    SERVER | REWRITE),
    op("XDEL",         WR, NG,   Some(N::Stream), None,    SERVER),
    op("XGROUP",       WR, GROW, Some(N::Stream), None,    SERVER | REWRITE),
    op("XINFO",        RD, NG,   None,            None,    SERVER),
    op("XLEN",         RD, NG,   None,            None,    SERVER),
    op("XPENDING",     RD, NG,   None,            None,    SERVER),
    op("XRANGE",       RD, NG,   None,            None,    SERVER),
    op("XREAD",        RD, NG,   None,            None,    SERVER),
    op("XREADGROUP",   WR, GROW, Some(N::Stream), None,    SERVER),
    op("XREVRANGE",    RD, NG,   None,            None,    SERVER),
    op("XSETID",       WR, NG,   Some(N::Stream), None,    SERVER | REWRITE),
    op("XTRIM",        WR, NG,   Some(N::Stream), None,    SERVER),
    // ---- geo (zset-backed, server-only today) --------------------------
    op("GEOADD",       WR, GROW, Some(N::Zset),   None,    SERVER),
    op("GEODIST",      RD, NG,   None,            None,    SERVER),
    op("GEOHASH",      RD, NG,   None,            None,    SERVER),
    op("GEOPOS",       RD, NG,   None,            None,    SERVER),
    op("GEORADIUS",    WR, GROW, None,            None,    SERVER),
    op("GEORADIUSBYMEMBER", WR, GROW, None,       None,    SERVER),
    op("GEOSEARCH",    RD, NG,   None,            None,    SERVER),
    op("GEOSEARCHSTORE", WR, GROW, None,          None,    SERVER),
    // ---- keyspace -------------------------------------------------------
    op("COPY",         WR, GROW, None,            None,    SERVER | ESTORE),
    op("DBSIZE",       RD, NG,   None,            None,    SERVER | ESTORE),
    // CDC surface: FEED.* / PREFIX.STATS are namespaced commands;
    // embedded parity = changes_since / changes_tail / feed_shards /
    // info_prefix.
    // Index engine (IDX.* namespace). CREATE/DROP mutate the
    // catalog (sidecar-persisted, not data writes — no AOF/replay);
    // reads ride the extension fan-out.
    // NB: catalog mutations are deliberately NOT data writes — the
    // `write` column tracks the AOF/propagation path, and the catalog
    // persists via its own sidecar (indexes are derived state).
    op("IDX.CREATE",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.DROP",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.LIST",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.ADVISE",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.QUERY",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.COUNT",    RD, NG,   None,            None,    SERVER | ESTORE),
    // IDX.VERIFY: server-only (embedded exposes idx_stats instead).
    op("IDX.VERIFY",   RD, NG,   None,            None,    SERVER),
    op("IDX.EXPLAIN",  RD, NG,   None,            None,    SERVER),
    // Views (VIEW.* namespace; catalog ops are sidecar-persisted,
    // not data writes — same reasoning as IDX.*). VERIFY/REBUILD/
    // EXPLAIN are server-only (embedded rebuilds inline and exposes
    // view_count instead).
    op("IDX.REBUILD",  RD, NG,   None,            None,    SERVER),
    op("PREFIX.DIGEST", RD, NG,  None,            None,    SERVER | ESTORE),
    // Tables (the TABLE.* namespace). DECLARE compiles
    // to IDX specs at declare time; catalog ops are sidecar-persisted,
    // not data writes — same reasoning as IDX.*.
    op("TABLE.DECLARE", RD, NG,  None,            None,    SERVER | ESTORE),
    op("TABLE.ENSURE", RD, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.REPLACE", RD, NG,  None,            None,    SERVER | ESTORE),
    op("TABLE.DROP",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.LIST",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.VERIFY", RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.CREATE",  RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.DROP",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.LIST",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.QUERY",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.VERIFY",  RD, NG,   None,            None,    SERVER),
    op("VIEW.REBUILD", RD, NG,   None,            None,    SERVER),
    op("VIEW.EXPLAIN", RD, NG,   None,            None,    SERVER),
    op("FEED.READ",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("FEED.TAIL",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("FEED.SHARDS",  RD, NG,   None,            None,    SERVER | ESTORE),
    op("PREFIX.STATS", RD, NG,   None,            None,    SERVER | ESTORE),
    op("DEL",          WR, NG,   Some(N::Generic), None,   SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("EXISTS",       RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("EXPIRE",       WR, NG,   Some(N::Generic), None,   SERVER | ESTORE | REPLAY),
    op("EXPIREAT",     WR, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("FLUSHALL",     WR, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("FLUSHDB",      WR, NG,   None,            None,    SERVER | REPLAY),
    op("KEYS",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("PERSIST",      WR, NG,   Some(N::Generic), None,   SERVER | ESTORE | REPLAY),
    op("PEXPIRE",      WR, NG,   Some(N::Generic), None,   SERVER | ESTORE | REPLAY),
    op("PEXPIREAT",    WR, NG,   None,            None,    SERVER | ESTORE | REPLAY | REWRITE),
    op("PTTL",         RD, NG,   None,            None,    SERVER),
    op("RANDOMKEY",    RD, NG,   None,            None,    SERVER | ESTORE),
    // RENAME/RENAMENX are writes, but the server routes them at the
    // runtime Op level (Route::Rename → exec_op synthesis) — the
    // verb-level is_write classification is false by design; the
    // `write` column mirrors that contract literally.
    op("RENAME",       RD, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("RENAMENX",     RD, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("SCAN",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("TIME",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("TOUCH",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("TTL",          RD, NG,   None,            None,    SERVER | ESTORE),
    op("TYPE",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("UNLINK",       WR, NG,   Some(N::Generic), None,   SERVER | ESTORE),
];

/// A confirmed should-exist-but-doesn't hole: `(op, surface, reason)`.
/// Parity tests assert this ledger is EXACT — closing a gap without
/// removing its entry is a CI failure, so the ledger can only shrink
/// truthfully. Categories: F2 = replica-apply holes (replay verbs
/// missing), F3 = RESP-dispatch holes (facade exists, wire doesn't).
pub const KNOWN_GAPS: &[(&str, u16, &str)] = &[
    // F3 — exists in kevy-store + embedded but not on the server wire.
    ("BITOP",    surface::SERVER, "F3"),
    ("SSCAN",    surface::ESTORE, "manifest sweep 2026-07-03: scan/hscan/zscan facades exist, sscan missing"),
    // F2 — server-propagatable writes an embed-as-replica cannot
    // apply (replay verbs missing). Until closed, embed-as-replica is
    // only safe for the basic-type verb set.
    ("SETNX",      surface::REPLAY, "F2: replica-apply hole"),
    ("SETEX",      surface::REPLAY, "F2"),
    ("PSETEX",     surface::REPLAY, "F2"),
    ("MSET",       surface::REPLAY, "F2"),
    ("HMSET",      surface::REPLAY, "F2"),
    ("RPOPLPUSH",  surface::REPLAY, "F2"),
    ("BRPOPLPUSH", surface::REPLAY, "F2"),
    ("LMOVE",      surface::REPLAY, "F2"),
    ("BLPOP",      surface::REPLAY, "F2"),
    ("BRPOP",      surface::REPLAY, "F2"),
    ("BZPOPMIN",   surface::REPLAY, "F2"),
    ("UNLINK",     surface::REPLAY, "F2"),
    ("GETEX",      surface::REPLAY, "F2: embedded getex TTL side-effect logs?  verify at closure"),
    ("GEOADD",     surface::REPLAY, "F2"),
    ("GEOSEARCHSTORE", surface::REPLAY, "F2"),
    ("GEORADIUS",  surface::REPLAY, "F2"),
    ("GEORADIUSBYMEMBER", surface::REPLAY, "F2"),
    ("XADD",       surface::REPLAY, "F2"),
    ("XDEL",       surface::REPLAY, "F2"),
    ("XTRIM",      surface::REPLAY, "F2"),
    ("XSETID",     surface::REPLAY, "F2"),
    ("XGROUP",     surface::REPLAY, "F2"),
    ("XREADGROUP", surface::REPLAY, "F2"),
    ("XACK",       surface::REPLAY, "F2"),
    ("XCLAIM",     surface::REPLAY, "F2"),
    ("XAUTOCLAIM", surface::REPLAY, "F2"),
];

/// Every op name carrying `flag` in its surface bitset.
pub fn ops_with(flag: u16) -> Vec<&'static str> {
    OP_TABLE.iter().filter(|o| o.surfaces & flag != 0).map(|o| o.name).collect()
}

/// Look up a row by canonical (uppercase) name.
pub fn spec(name: &str) -> Option<&'static OpSpec> {
    OP_TABLE.iter().find(|o| o.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for o in OP_TABLE {
            assert!(seen.insert(o.name), "duplicate OP_TABLE row: {}", o.name);
        }
    }

    #[test]
    fn growing_implies_write_and_wake_implies_write() {
        for o in OP_TABLE {
            if o.growing {
                assert!(o.write, "{}: growing but not write", o.name);
            }
            if o.wake_idx.is_some() {
                assert!(o.write, "{}: wakes waiters but not write", o.name);
            }
        }
    }

    #[test]
    fn known_gaps_reference_real_ops_and_are_actual_holes() {
        for (name, flag, _) in KNOWN_GAPS {
            let s = spec(name).unwrap_or_else(|| panic!("gap entry for unknown op {name}"));
            assert_eq!(
                s.surfaces & flag,
                0,
                "{name}: KNOWN_GAPS says surface {flag:#b} is missing, but the table has the bit set — \
                 the gap was closed; remove the ledger entry"
            );
        }
    }

    #[test]
    fn every_logged_verb_is_replayable() {
        // The no-silent-data-loss invariant: an op present on any embedded write
        // surface (facade/pipe/atomic) that is a write MUST have a
        // replay arm — unless it is explicitly ledgered.
        for o in OP_TABLE {
            let on_embedded_write =
                o.write && o.surfaces & (surface::ESTORE | surface::PIPE | surface::ATOMIC) != 0;
            if !on_embedded_write {
                continue;
            }
            let replayable = o.surfaces & surface::REPLAY != 0;
            let ledgered = KNOWN_GAPS
                .iter()
                .any(|(n, f, _)| n == &o.name && f & surface::REPLAY != 0);
            // Ops whose AOF form is a DIFFERENT verb (documented effect
            // logging): SPOP→SREM handled by SPOP retaining REPLAY for
            // legacy frames; MSET/SETNX/GETEX log SET/PEXPIREAT forms.
            let logs_as_other_verb = matches!(
                o.name,
                "MSET" | "SETNX" | "GETEX" | "BITOP" | "COPY" | "UNLINK" | "TOUCH"
                    // Algebra stores: effect-logged as DEL + plain ZADD/SADD.
                    | "ZINTERSTORE" | "ZUNIONSTORE" | "ZDIFFSTORE"
                    | "ZPOPMIN.BELOW"
                    | "HEXPIRE" | "HPEXPIRE"
                    | "SINTERSTORE" | "SUNIONSTORE" | "SDIFFSTORE"
            );
            assert!(
                replayable || ledgered || logs_as_other_verb,
                "{}: embedded write surface without a replay arm and not ledgered — \
                 this is the silent-data-loss-on-reopen class",
                o.name
            );
        }
    }
}
