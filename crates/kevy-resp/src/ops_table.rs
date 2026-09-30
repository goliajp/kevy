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
//!
//! ```
//! use kevy_resp::ops_table::{NotifyKind, spec, surface};
//!
//! let row = spec("RPUSH").expect("registered");
//! assert!(row.write && row.growing);
//! assert_eq!(row.notify, Some(NotifyKind::List));
//! assert_eq!(row.wake_idx, Some(1));
//! assert_ne!(row.surfaces & surface::SERVER, 0);
//! ```

#[path = "ops_surface.rs"]
pub mod surface;

/// Keyspace-notification class of a command (the Redis class letter
/// each variant names).
///
/// ```
/// use kevy_resp::ops_table::{NotifyKind, spec};
/// assert_eq!(spec("LPUSH").and_then(|s| s.notify), Some(NotifyKind::List));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NotifyKind {
    /// Redis notification class `$` (string commands).
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("APPEND").unwrap().notify, Some(NotifyKind::String));
    /// ```
    String,
    /// Class `h`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("HSET").unwrap().notify, Some(NotifyKind::Hash));
    /// ```
    Hash,
    /// Class `l`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("RPUSH").unwrap().notify, Some(NotifyKind::List));
    /// ```
    List,
    /// Class `s`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("SADD").unwrap().notify, Some(NotifyKind::Set));
    /// ```
    Set,
    /// Class `z`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("ZADD").unwrap().notify, Some(NotifyKind::Zset));
    /// ```
    Zset,
    /// Class `t`.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("XADD").unwrap().notify, Some(NotifyKind::Stream));
    /// ```
    Stream,
    /// Class `g` (DEL / EXPIRE / PERSIST …).
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("DEL").unwrap().notify, Some(NotifyKind::Generic));
    /// ```
    Generic,
}

/// One registry row: a command's classification + the surfaces it
/// exists on today. The rows are the crate's own [`OP_TABLE`]; read them
/// with [`spec`].
///
/// ```
/// let get = kevy_resp::ops_table::spec("GET").unwrap();
/// assert!(!get.write && get.notify.is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OpSpec {
    /// Canonical uppercase command name.
    ///
    /// ```
    /// use kevy_resp::ops_table::spec;
    /// assert_eq!(spec("GET").unwrap().name, "GET");
    /// // lookups are by the canonical uppercase form
    /// assert!(spec("get").is_none());
    /// ```
    pub name: &'static str,
    /// Server `is_write_verb` classification (AOF/replication gate).
    ///
    /// ```
    /// use kevy_resp::ops_table::spec;
    /// assert!(spec("SET").unwrap().write);
    /// assert!(!spec("GET").unwrap().write);
    /// ```
    pub write: bool,
    /// Subset of `write` that can grow memory (OOM precheck).
    ///
    /// ```
    /// use kevy_resp::ops_table::spec;
    /// assert!(spec("SET").unwrap().growing);
    /// // DEL writes but only frees memory
    /// let del = spec("DEL").unwrap();
    /// assert!(del.write && !del.growing);
    /// ```
    pub growing: bool,
    /// Keyspace-notification class; `None` = no notification.
    ///
    /// ```
    /// use kevy_resp::ops_table::{NotifyKind, spec};
    /// assert_eq!(spec("SET").unwrap().notify, Some(NotifyKind::String));
    /// assert_eq!(spec("GET").unwrap().notify, None);
    /// ```
    pub notify: Option<NotifyKind>,
    /// Producer verbs that wake blocked waiters: key arg index.
    ///
    /// ```
    /// use kevy_resp::ops_table::spec;
    /// // LPUSH key ... wakes BLPOP waiters on argv[1]
    /// assert_eq!(spec("LPUSH").unwrap().wake_idx, Some(1));
    /// assert_eq!(spec("SET").unwrap().wake_idx, None);
    /// ```
    pub wake_idx: Option<u8>,
    /// Bitset of [`surface`] flags where the op exists today.
    ///
    /// ```
    /// use kevy_resp::ops_table::{spec, surface};
    /// let xadd = spec("XADD").unwrap();
    /// assert_ne!(xadd.surfaces & surface::SERVER, 0);
    /// assert_eq!(xadd.surfaces & surface::ESTORE, 0);
    /// ```
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

/// The internal record verb that carries a stream consumer's last contact
/// with its group: `XINTERNAL.CONSUMERSEEN key group consumer unix-ms`.
/// kevy writes it to the AOF, the replication stream and the feed, and
/// applies it on replay and on a replica; a client that sends it is
/// refused. It is not a Redis command and is not documented as one.
///
/// ```
/// use kevy_resp::ops_table::{CONSUMER_SEEN, spec, surface};
/// let row = spec(CONSUMER_SEEN).unwrap();
/// assert!(row.write && row.surfaces == surface::REPLAY, "applied, never served");
/// ```
pub const CONSUMER_SEEN: &str = "XINTERNAL.CONSUMERSEEN";

/// The internal record verb that carries the whole index, view and table
/// catalog: `XINTERNAL.CATALOG lineage version index view table`. Every
/// catalog command is recorded as one, and every snapshot and rewritten
/// log keeps the current one; replay and a replica apply it when it is
/// newer than what they hold. A client that sends it is refused.
///
/// ```
/// use kevy_resp::ops_table::{CATALOG, spec, surface};
/// let row = spec(CATALOG).unwrap();
/// assert!(row.write && row.surfaces & surface::SERVER == 0, "never served");
/// ```
pub const CATALOG: &str = "XINTERNAL.CATALOG";

/// The registry. One row per command. Kept grouped by type family and
/// alphabetical inside each group so a missing row is easy to spot.
///
/// ```
/// use kevy_resp::ops_table::OP_TABLE;
/// assert!(OP_TABLE.iter().any(|o| o.name == "SET"));
/// assert!(OP_TABLE.iter().all(|o| !o.growing || o.write));
/// ```
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
    op("GETEX",        WR, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("GETRANGE",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("GETSET",       WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("INCR",         WR, GROW, Some(N::String), None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("INCRBY",       WR, GROW, Some(N::String), None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("INCRBYFLOAT",  WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("MGET",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("MSET",         WR, GROW, None,            None,    SERVER | ESTORE | REPLAY),
    op("PSETEX",       WR, GROW, Some(N::String), None,    SERVER | REPLAY),
    op("SET",          WR, GROW, Some(N::String), None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    op("SETEX",        WR, GROW, Some(N::String), None,    SERVER | REPLAY),
    op("SETNX",        WR, GROW, Some(N::String), None,    SERVER | ESTORE | REPLAY),
    op("SETRANGE",     WR, GROW, Some(N::String),            None,    SERVER | ESTORE | REPLAY),
    op("STRLEN",       RD, NG,   None,            None,    SERVER | ESTORE),
    // ---- bitmap (string-backed) ---------------------------------------
    op("BITCOUNT",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("BITOP",        WR, GROW, None,            None,    SERVER | ESTORE),
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
    op("HRANDFIELD",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("HKEYS",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("HLEN",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("HMGET",        RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("HMSET",        WR, GROW, Some(N::Hash),   None,    SERVER | REPLAY),
    op("HSCAN",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("HSET",         WR, GROW, Some(N::Hash),   None,    SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    // Hash field TTLs (Redis 7.4). A relative form is followed in the
    // log by the absolute HPEXPIREAT it set, the replay/rewrite carrier.
    op("HEXPIRE",      WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | REPLAY),
    op("HPEXPIRE",     WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | REPLAY),
    op("HPEXPIREAT",   WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | REPLAY | REWRITE),
    op("HTTL",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("HPTTL",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("HPERSIST",     WR, NG,   Some(N::Hash),   None,    SERVER | ESTORE | REPLAY),
    op("HSETNX",       WR, GROW, Some(N::Hash),   None,    SERVER | ESTORE | REPLAY),
    op("HVALS",        RD, NG,   None,            None,    SERVER | ESTORE),
    // ---- lists --------------------------------------------------------
    // BLPOP/BRPOP write when they pop, and record the pop as a plain
    // LPOP/RPOP.
    op("BLPOP",        WR, NG,   None,            None,    SERVER | REPLAY),
    op("BRPOP",        WR, NG,   None,            None,    SERVER | REPLAY),
    // Blocking form notifies via its executed effect, not the verb.
    op("BRPOPLPUSH",   WR, GROW, None,            None,    SERVER | REPLAY),
    op("LINDEX",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("LINSERT",      WR, GROW, Some(N::List),            None,    SERVER | ESTORE | REPLAY),
    op("LLEN",         RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("LMOVE",        WR, GROW, Some(N::List),   None,    SERVER | REPLAY),
    op("LPOP",         WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("LPOS",         RD, NG,   None,            None,    SERVER),
    op("LPUSH",        WR, GROW, Some(N::List),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    op("LRANGE",       RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("LREM",         WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("LSET",         WR, GROW, Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("LTRIM",        WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("RPOP",         WR, NG,   Some(N::List),   None,    SERVER | ESTORE | REPLAY),
    op("RPOPLPUSH",    WR, GROW, Some(N::List),   None,    SERVER | REPLAY),
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
    op("BZPOPMIN",     WR, NG,   None,            None,    SERVER | REPLAY),
    op("ZADD",         WR, GROW, Some(N::Zset),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY | REWRITE),
    op("ZCARD",        RD, NG,   None,            None,    SERVER | ESTORE | ATOMIC),
    op("ZCOUNT",       RD, NG,   None,            None,    SERVER | ESTORE),
    op("ZINCRBY",      WR, GROW, Some(N::Zset),   Some(1), SERVER | ESTORE | PIPE | ATOMIC | REPLAY),
    // Set/zset algebra stores: effect-logged as DEL+ZADD/SADD, so no
    // REPLAY arm of their own is needed (the effect verbs replay).
    op("ZINTERSTORE",  WR, GROW, Some(N::Zset),   None,    SERVER | ESTORE),
    // Delayed-job primitive; embedded logs the ZREM effect.
    op("ZPOPMIN.BELOW", WR, NG,  Some(N::Zset),   None,    SERVER | ESTORE | REPLAY),
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
    // ---- streams (embedded replay with its streams-geo feature) --------
    op("XACK",         WR, NG,   Some(N::Stream), None,    SERVER | REPLAY),
    op("XADD",         WR, GROW, Some(N::Stream), Some(1), SERVER | REPLAY | REWRITE),
    op("XAUTOCLAIM",   WR, GROW, Some(N::Stream), None,    SERVER | REPLAY),
    op("XCLAIM",       WR, GROW, Some(N::Stream), None,    SERVER | REPLAY | REWRITE),
    op("XDEL",         WR, NG,   Some(N::Stream), None,    SERVER | REPLAY),
    op("XGROUP",       WR, GROW, Some(N::Stream), None,    SERVER | REPLAY | REWRITE),
    op("XINFO",        RD, NG,   None,            None,    SERVER),
    op("XLEN",         RD, NG,   None,            None,    SERVER),
    op("XPENDING",     RD, NG,   None,            None,    SERVER),
    op("XRANGE",       RD, NG,   None,            None,    SERVER),
    op("XREAD",        RD, NG,   None,            None,    SERVER),
    op("XREADGROUP",   WR, GROW, Some(N::Stream), None,    SERVER | REPLAY),
    op("XREVRANGE",    RD, NG,   None,            None,    SERVER),
    op("XSETID",       WR, NG,   Some(N::Stream), None,    SERVER | REPLAY | REWRITE),
    op("XTRIM",        WR, NG,   Some(N::Stream), None,    SERVER | REPLAY),
    // internal: applied from a record, refused from a client
    op(CONSUMER_SEEN,  WR, NG,   None,            None,    REPLAY),
    op(CATALOG,        WR, NG,   None,            None,    0),
    // ---- geo (zset-backed; embedded replay as streams) ----------------
    op("GEOADD",       WR, GROW, Some(N::Zset),   None,    SERVER | REPLAY),
    op("GEODIST",      RD, NG,   None,            None,    SERVER),
    op("GEOHASH",      RD, NG,   None,            None,    SERVER),
    op("GEOPOS",       RD, NG,   None,            None,    SERVER),
    op("GEORADIUS",    WR, GROW, None,            None,    SERVER | REPLAY),
    op("GEORADIUSBYMEMBER", WR, GROW, None,       None,    SERVER | REPLAY),
    op("GEOSEARCH",    RD, NG,   None,            None,    SERVER),
    op("GEOSEARCHSTORE", WR, GROW, None,          None,    SERVER | REPLAY),
    // ---- keyspace -------------------------------------------------------
    op("COPY",         WR, GROW, None,            None,    SERVER | ESTORE),
    op("DBSIZE",       RD, NG,   None,            None,    SERVER | ESTORE),
    // CDC surface: FEED.* / PREFIX.STATS are namespaced commands;
    // embedded parity = changes_since / changes_tail / feed_shards /
    // info_prefix.
    // Index engine (IDX.* namespace). CREATE/DROP/REBUILD change the
    // catalog and are recorded as the whole catalog (an XINTERNAL.CATALOG
    // frame); reads ride the extension fan-out.
    op("IDX.CREATE",   WR, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.DROP",     WR, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.LIST",     RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.ADVISE",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.DESCRIBE", RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.QUERY",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("IDX.COUNT",    RD, NG,   None,            None,    SERVER | ESTORE),
    // IDX.VERIFY: server-only (embedded exposes idx_stats instead).
    op("IDX.VERIFY",   RD, NG,   None,            None,    SERVER),
    op("IDX.EXPLAIN",  RD, NG,   None,            None,    SERVER),
    // Views (VIEW.* namespace; catalog ops are recorded like IDX.*).
    // VERIFY/REBUILD/EXPLAIN are server-only (embedded rebuilds inline
    // and exposes view_count instead).
    op("IDX.REBUILD",  WR, NG,   None,            None,    SERVER),
    op("PREFIX.DIGEST", RD, NG,  None,            None,    SERVER | ESTORE),
    // Tables (the TABLE.* namespace). DECLARE compiles
    // to IDX specs at declare time; catalog ops are recorded like IDX.*.
    op("TABLE.DECLARE", WR, NG,  None,            None,    SERVER | ESTORE),
    op("TABLE.ENSURE", WR, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.REPLACE", WR, NG,  None,            None,    SERVER | ESTORE),
    op("TABLE.DROP",   WR, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.LIST",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.VERIFY", RD, NG,   None,            None,    SERVER | ESTORE),
    op("TABLE.DESCRIBE", RD, NG, None,            None,    SERVER | ESTORE),
    op("VIEW.CREATE",  WR, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.DROP",    WR, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.LIST",    RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.QUERY",   RD, NG,   None,            None,    SERVER | ESTORE),
    op("VIEW.DESCRIBE", RD, NG,  None,            None,    SERVER | ESTORE),
    op("VIEW.VERIFY",  RD, NG,   None,            None,    SERVER),
    op("VIEW.REBUILD", WR, NG,   None,            None,    SERVER),
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
    // The server runs RENAME/RENAMENX at the runtime Op level
    // (Route::Rename), which records the move itself.
    op("RENAME",       WR, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("RENAMENX",     WR, NG,   None,            None,    SERVER | ESTORE | REPLAY),
    op("SCAN",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("TIME",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("TOUCH",        RD, NG,   None,            None,    SERVER | ESTORE),
    op("TTL",          RD, NG,   None,            None,    SERVER | ESTORE),
    op("TYPE",         RD, NG,   None,            None,    SERVER | ESTORE),
    op("UNLINK",       WR, NG,   Some(N::Generic), None,   SERVER | ESTORE | REPLAY),
];

/// A confirmed should-exist-but-doesn't hole: `(op, surface, reason)`.
/// Parity tests assert this ledger is EXACT — closing a gap without
/// removing its entry is a CI failure, so the ledger can only shrink
/// truthfully. Categories: F2 = replica-apply holes (replay verbs
/// missing), F3 = RESP-dispatch holes (facade exists, wire doesn't).
///
/// ```
/// use kevy_resp::ops_table::{KNOWN_GAPS, spec};
/// for (name, flag, _why) in KNOWN_GAPS {
///     // a ledgered gap is a surface bit the row really lacks
///     assert_eq!(spec(name).unwrap().surfaces & flag, 0);
/// }
/// ```
pub const KNOWN_GAPS: &[(&str, u16, &str)] = &[
    // F3 — exists in kevy-store + embedded but not on the server wire.
    (
        "SSCAN",
        surface::ESTORE,
        "manifest sweep 2026-07-03: scan/hscan/zscan facades exist, sscan missing",
    ),
];

/// Every op name carrying `flag` in its surface bitset.
///
/// ```
/// use kevy_resp::ops_table::{ops_with, surface};
/// let replayed = ops_with(surface::REPLAY);
/// assert!(replayed.contains(&"SET"));
/// assert!(!replayed.contains(&"GET"));
/// ```
pub fn ops_with(flag: u16) -> Vec<&'static str> {
    OP_TABLE.iter().filter(|o| o.surfaces & flag != 0).map(|o| o.name).collect()
}

/// Look up a row by canonical (uppercase) name.
///
/// ```
/// use kevy_resp::ops_table::spec;
/// assert!(spec("HSET").is_some_and(|s| s.write));
/// assert!(spec("NOSUCHCMD").is_none());
/// ```
pub fn spec(name: &str) -> Option<&'static OpSpec> {
    OP_TABLE.iter().find(|o| o.name == name)
}

#[cfg(test)]
#[path = "ops_table_tests.rs"]
mod tests;
