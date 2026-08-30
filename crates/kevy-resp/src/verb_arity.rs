//! Verb arity — the copy of the verb table that a crate outside `kevy`
//! can read.
//!
//! `kevy::verb_meta` is the documentation registry: syntax, complexity,
//! the Redis comparison, and arity. It lives in `kevy`, which is cement,
//! so `kevy-embedded` — steel — may not depend on it, and until now the
//! facade restated the arity numbers it needed as literals in its own
//! entry checks. Two of them, `argv.len() < 4` for IDX.QUERY and
//! IDX.COUNT, each beside a comment saying the server's arity is -4.
//!
//! That is a drift waiting to happen: the wire differential harness had
//! already caught the two surfaces answering the same typo in two
//! different sentences once. A literal next to a comment naming its
//! source is the same shape, one step earlier.
//!
//! So the arity column lives here too, where both surfaces can read it,
//! and `kevy`'s parity tests hold the two tables in exact bijection —
//! same names, same numbers, checked both ways. A verb added to one and
//! not the other fails there by name.
//!
//! Only arity is duplicated, not the registry. The rest of a row is
//! documentation prose: 48 KB of it, which every wasm build linking this
//! crate would carry for nothing.
//!
//! ```
//! use kevy_resp::verb_arity::arity_ok;
//!
//! // What a dispatch entry does with it, instead of writing `< 4` beside
//! // a comment explaining that -4 means four.
//! fn refuses(argc: usize) -> bool {
//!     arity_ok("IDX.QUERY", argc) == Some(false)
//! }
//! assert!(refuses(3));
//! assert!(!refuses(4));
//! ```

/// Redis's arity convention: positive is an exact argument count
/// INCLUDING the verb itself, negative is a minimum. `-4` means "at
/// least four". Sorted by name, which [`arity_of`] relies on.
///
/// ```
/// use kevy_resp::verb_arity::VERB_ARITY;
///
/// // Sorted, so a lookup can binary-search it.
/// assert!(VERB_ARITY.windows(2).all(|w| w[0].0 < w[1].0));
/// // GET takes the verb and one key, exactly.
/// assert!(VERB_ARITY.contains(&("GET", 2)));
/// ```
pub const VERB_ARITY: &[(&str, i8)] = &[
    ("APPEND", 3),
    ("BGREWRITEAOF", 1),
    ("BGSAVE", 1),
    ("BITCOUNT", -2),
    ("BITOP", -4),
    ("BITPOS", -3),
    ("BLPOP", -3),
    ("BRPOP", -3),
    ("BRPOPLPUSH", 4),
    ("BZPOPMIN", -3),
    ("CLIENT", -2),
    ("CLUSTER", -2),
    ("COMMAND", -1),
    ("CONFIG", -2),
    ("COPY", -3),
    ("DBSIZE", 1),
    ("DEBUG", -2),
    ("DECR", 2),
    ("DECRBY", 3),
    ("DEL", -2),
    ("DISCARD", 1),
    ("ECHO", 2),
    ("EVAL", -3),
    ("EVALSHA", -3),
    ("EXEC", 1),
    ("EXISTS", -2),
    ("EXPIRE", 3),
    ("EXPIREAT", 3),
    ("FAILOVER", -3),
    ("FEED.READ", -4),
    ("FEED.SHARDS", 1),
    ("FEED.TAIL", 2),
    ("FLUSHALL", -1),
    ("FLUSHDB", -1),
    ("GEOADD", -5),
    ("GEODIST", -4),
    ("GEOHASH", -3),
    ("GEOPOS", -3),
    ("GEORADIUS", -6),
    ("GEORADIUSBYMEMBER", -5),
    ("GEOSEARCH", -4),
    ("GEOSEARCHSTORE", -5),
    ("GET", 2),
    ("GETBIT", 3),
    ("GETDEL", 2),
    ("GETEX", -2),
    ("GETRANGE", 4),
    ("GETSET", 3),
    ("HDEL", -3),
    ("HELLO", -1),
    ("HEXISTS", 3),
    ("HEXPIRE", -6),
    ("HGET", 3),
    ("HGETALL", 2),
    ("HINCRBY", 4),
    ("HINCRBYFLOAT", 4),
    ("HKEYS", 2),
    ("HLEN", 2),
    ("HMGET", -3),
    ("HMSET", -4),
    ("HPERSIST", -5),
    ("HPEXPIRE", -6),
    ("HPEXPIREAT", -6),
    ("HPTTL", -5),
    ("HSCAN", -3),
    ("HSET", -4),
    ("HSETNX", 4),
    ("HTTL", -5),
    ("HVALS", 2),
    ("IDX.ADVISE", 1),
    ("IDX.COUNT", -4),
    ("IDX.CREATE", -11),
    ("IDX.DROP", 2),
    ("IDX.EXPLAIN", -2),
    ("IDX.LIST", 1),
    ("IDX.QUERY", -4),
    ("IDX.REBUILD", 2),
    ("IDX.VERIFY", 2),
    ("INCR", 2),
    ("INCRBY", 3),
    ("INCRBYFLOAT", 3),
    ("INFO", -1),
    ("KEYS", 2),
    ("LINDEX", 3),
    ("LINSERT", 5),
    ("LLEN", 2),
    ("LMOVE", 5),
    ("LPOP", -2),
    ("LPOS", -3),
    ("LPUSH", -3),
    ("LRANGE", 4),
    ("LREM", 4),
    ("LSET", 4),
    ("LTRIM", 4),
    ("MEMORY", -2),
    ("MGET", -2),
    ("MSET", -3),
    ("MULTI", 1),
    ("PERSIST", 2),
    ("PEXPIRE", 3),
    ("PEXPIREAT", 3),
    ("PING", -1),
    ("PREFIX.DIGEST", 2),
    ("PREFIX.STATS", 2),
    ("PSETEX", 4),
    ("PSUBSCRIBE", -2),
    ("PTTL", 2),
    ("PUBLISH", 3),
    ("PUNSUBSCRIBE", -1),
    ("QUIT", 1),
    ("RANDOMKEY", 1),
    ("RENAME", 3),
    ("RENAMENX", 3),
    ("REPL.TOKEN", 1),
    ("REPL.WAIT", -3),
    ("REPLICAOF", 3),
    ("ROLE", 1),
    ("RPOP", -2),
    ("RPOPLPUSH", 3),
    ("RPUSH", -3),
    ("SADD", -3),
    ("SAVE", 1),
    ("SCAN", -2),
    ("SCARD", 2),
    ("SCRIPT", -2),
    ("SDIFF", -2),
    ("SDIFFSTORE", -3),
    ("SELECT", 2),
    ("SET", -3),
    ("SETBIT", 4),
    ("SETEX", 4),
    ("SETNX", 3),
    ("SETRANGE", 4),
    ("SHUTDOWN", -1),
    ("SINTER", -2),
    ("SINTERSTORE", -3),
    ("SISMEMBER", 3),
    ("SLAVEOF", 3),
    ("SLOWLOG", -2),
    ("SMEMBERS", 2),
    ("SPOP", -2),
    ("SRANDMEMBER", -2),
    ("SREM", -3),
    ("SSCAN", -3),
    ("STRLEN", 2),
    ("SUBSCRIBE", -2),
    ("SUNION", -2),
    ("SUNIONSTORE", -3),
    ("TABLE.DECLARE", -9),
    ("TABLE.DROP", 2),
    ("TABLE.ENSURE", -9),
    ("TABLE.LIST", 1),
    ("TABLE.REPLACE", -9),
    ("TABLE.VERIFY", 2),
    ("TIME", 1),
    ("TOUCH", -2),
    ("TTL", 2),
    ("TYPE", 2),
    ("UNLINK", -2),
    ("UNSUBSCRIBE", -1),
    ("UNWATCH", 1),
    ("VIEW.CREATE", -8),
    ("VIEW.DROP", 2),
    ("VIEW.EXPLAIN", 2),
    ("VIEW.LIST", 1),
    ("VIEW.QUERY", -2),
    ("VIEW.REBUILD", 2),
    ("VIEW.VERIFY", 2),
    ("WAIT", 3),
    ("WATCH", -2),
    ("XACK", -4),
    ("XADD", -5),
    ("XAUTOCLAIM", -6),
    ("XCLAIM", -6),
    ("XDEL", -3),
    ("XGROUP", -2),
    ("XINFO", -2),
    ("XLEN", 2),
    ("XPENDING", -3),
    ("XRANGE", -4),
    ("XREAD", -4),
    ("XREADGROUP", -7),
    ("XREVRANGE", -4),
    ("XSETID", -3),
    ("XTRIM", -4),
    ("ZADD", -4),
    ("ZCARD", 2),
    ("ZCOUNT", 4),
    ("ZDIFFSTORE", -4),
    ("ZINCRBY", 4),
    ("ZINTERCARD", -3),
    ("ZINTERSTORE", -4),
    ("ZPOPMIN", -2),
    ("ZPOPMIN.BELOW", -3),
    ("ZRANGE", -4),
    ("ZRANGEBYSCORE", -4),
    ("ZRANK", 3),
    ("ZREM", -3),
    ("ZREMRANGEBYRANK", 4),
    ("ZREMRANGEBYSCORE", 4),
    ("ZREVRANGE", -4),
    ("ZREVRANGEBYSCORE", -4),
    ("ZSCAN", -3),
    ("ZSCORE", 3),
    ("ZUNIONSTORE", -4),
];

/// The declared arity of `name` (upper-case, as the tables spell it), or
/// `None` when the verb has no row.
///
/// ```
/// use kevy_resp::verb_arity::arity_of;
///
/// assert_eq!(arity_of("GET"), Some(2));        // exactly two parts
/// assert_eq!(arity_of("IDX.QUERY"), Some(-4)); // at least four
/// assert_eq!(arity_of("get"), None);           // upper-case, as spelled
/// ```
#[must_use]
pub fn arity_of(name: &str) -> Option<i8> {
    VERB_ARITY.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| VERB_ARITY[i].1)
}

/// Whether `argc` — the whole argv INCLUDING the verb — satisfies the
/// declared arity. `None` when the verb has no row, so a caller cannot
/// read "no such verb" as "the count is fine".
///
/// This is what a dispatch entry wants. It keeps the sign convention in
/// one place instead of at every guard, which is how `< 4` came to be
/// written out beside a comment explaining that -4 means four.
///
/// ```
/// use kevy_resp::verb_arity::arity_ok;
///
/// assert_eq!(arity_ok("GET", 2), Some(true));        // GET k
/// assert_eq!(arity_ok("GET", 3), Some(false));       // GET k extra
/// assert_eq!(arity_ok("IDX.QUERY", 3), Some(false)); // one short
/// assert_eq!(arity_ok("IDX.QUERY", 9), Some(true));  // a minimum, so more is fine
/// assert_eq!(arity_ok("NO.SUCH.VERB", 2), None);     // not "the count is fine"
/// ```
#[must_use]
pub fn arity_ok(name: &str, argc: usize) -> Option<bool> {
    let a = i64::from(arity_of(name)?);
    let n = argc as i64;
    Some(if a < 0 { n >= -a } else { n == a })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_carries_no_zero_arity() {
        assert!(VERB_ARITY.len() > 150, "only {} rows — the table did not load", VERB_ARITY.len());
        for w in VERB_ARITY.windows(2) {
            assert!(w[0].0 < w[1].0, "{} and {} are out of order", w[0].0, w[1].0);
        }
        for (n, a) in VERB_ARITY {
            assert!(*a != 0, "{n}: arity 0 is meaningless");
        }
    }

    #[test]
    fn arity_ok_reads_both_signs_and_refuses_an_unknown_verb() {
        assert_eq!(arity_ok("GET", 2), Some(true));
        assert_eq!(arity_ok("GET", 3), Some(false));
        assert_eq!(arity_ok("IDX.QUERY", 3), Some(false));
        assert_eq!(arity_ok("IDX.QUERY", 4), Some(true));
        assert_eq!(arity_ok("IDX.QUERY", 40), Some(true));
        assert_eq!(arity_ok("NO.SUCH.VERB", 4), None);
    }
}
