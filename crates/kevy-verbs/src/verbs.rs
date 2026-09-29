//! The table of verbs [`crate::exec`] answers.

/// One verb the shared layer executes.
///
/// ```
/// let v = kevy_verbs::verb(b"GET").unwrap();
/// assert_eq!((v.name, v.write), ("GET", false));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Verb {
    /// The verb, uppercase.
    pub name: &'static str,
    /// Whether it can change the keyspace.
    pub write: bool,
}

const fn v(name: &'static str, write: bool) -> Verb {
    Verb { name, write }
}

const RD: bool = false;
const WR: bool = true;

/// Every verb [`crate::exec`] answers, sorted by name. The stream and geo
/// rows are there only with the `streams-geo` feature.
///
/// ```
/// let names: Vec<&str> = kevy_verbs::VERBS.iter().map(|v| v.name).collect();
/// assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted, so lookup can bisect");
/// assert!(kevy_verbs::VERBS.iter().any(|v| v.name == "LPUSH" && v.write));
/// ```
#[rustfmt::skip]
// LOC-WAIVER: pure data table — one row per shared verb.
pub const VERBS: &[Verb] = &[
    v("APPEND", WR),
    v("BITCOUNT", RD),
    v("BITPOS", RD),
    v("BLPOP", WR),
    v("BRPOP", WR),
    v("BRPOPLPUSH", WR),
    v("BZPOPMIN", WR),
    v("DBSIZE", RD),
    v("DECR", WR),
    v("DECRBY", WR),
    v("DEL", WR),
    v("EXISTS", RD),
    v("EXPIRE", WR),
    v("EXPIREAT", WR),
    v("FLUSHALL", WR),
    v("FLUSHDB", WR),
    #[cfg(feature = "streams-geo")]
    v("GEOADD", WR),
    #[cfg(feature = "streams-geo")]
    v("GEODIST", RD),
    #[cfg(feature = "streams-geo")]
    v("GEOHASH", RD),
    #[cfg(feature = "streams-geo")]
    v("GEOPOS", RD),
    #[cfg(feature = "streams-geo")]
    v("GEORADIUS", WR),
    #[cfg(feature = "streams-geo")]
    v("GEORADIUSBYMEMBER", WR),
    #[cfg(feature = "streams-geo")]
    v("GEOSEARCH", RD),
    #[cfg(feature = "streams-geo")]
    v("GEOSEARCHSTORE", WR),
    v("GET", RD),
    v("GETBIT", RD),
    v("GETDEL", WR),
    v("GETEX", WR),
    v("GETRANGE", RD),
    v("GETSET", WR),
    v("HDEL", WR),
    v("HEXISTS", RD),
    v("HEXPIRE", WR),
    v("HGET", RD),
    v("HGETALL", RD),
    v("HINCRBY", WR),
    v("HINCRBYFLOAT", WR),
    v("HKEYS", RD),
    v("HLEN", RD),
    v("HMGET", RD),
    v("HMSET", WR),
    v("HPERSIST", WR),
    v("HPEXPIRE", WR),
    v("HPEXPIREAT", WR),
    v("HPTTL", RD),
    v("HRANDFIELD", RD),
    v("HSCAN", RD),
    v("HSET", WR),
    v("HSETNX", WR),
    v("HTTL", RD),
    v("HVALS", RD),
    v("INCR", WR),
    v("INCRBY", WR),
    v("INCRBYFLOAT", WR),
    v("LINDEX", RD),
    v("LINSERT", WR),
    v("LLEN", RD),
    v("LMOVE", WR),
    v("LPOP", WR),
    v("LPOS", RD),
    v("LPUSH", WR),
    v("LRANGE", RD),
    v("LREM", WR),
    v("LSET", WR),
    v("LTRIM", WR),
    v("MSET", WR),
    v("PERSIST", WR),
    v("PEXPIRE", WR),
    v("PEXPIREAT", WR),
    v("PSETEX", WR),
    v("PTTL", RD),
    v("RENAME", WR),
    v("RENAMENX", WR),
    v("RPOP", WR),
    v("RPOPLPUSH", WR),
    v("RPUSH", WR),
    v("SADD", WR),
    v("SCARD", RD),
    v("SET", WR),
    v("SETBIT", WR),
    v("SETEX", WR),
    v("SETNX", WR),
    v("SETRANGE", WR),
    v("SISMEMBER", RD),
    v("SMEMBERS", RD),
    v("SPOP", WR),
    v("SRANDMEMBER", RD),
    v("SREM", WR),
    v("SSCAN", RD),
    v("STRLEN", RD),
    v("TOUCH", RD),
    v("TTL", RD),
    v("TYPE", RD),
    v("UNLINK", WR),
    #[cfg(feature = "streams-geo")]
    v("XACK", WR),
    #[cfg(feature = "streams-geo")]
    v("XADD", WR),
    #[cfg(feature = "streams-geo")]
    v("XAUTOCLAIM", WR),
    #[cfg(feature = "streams-geo")]
    v("XCLAIM", WR),
    #[cfg(feature = "streams-geo")]
    v("XDEL", WR),
    #[cfg(feature = "streams-geo")]
    v("XGROUP", WR),
    #[cfg(feature = "streams-geo")]
    v("XINFO", RD),
    #[cfg(feature = "streams-geo")]
    v("XLEN", RD),
    #[cfg(feature = "streams-geo")]
    v("XPENDING", RD),
    #[cfg(feature = "streams-geo")]
    v("XRANGE", RD),
    #[cfg(feature = "streams-geo")]
    v("XREAD", RD),
    #[cfg(feature = "streams-geo")]
    v("XREADGROUP", WR),
    #[cfg(feature = "streams-geo")]
    v("XREVRANGE", RD),
    #[cfg(feature = "streams-geo")]
    v("XSETID", WR),
    #[cfg(feature = "streams-geo")]
    v("XTRIM", WR),
    v("ZADD", WR),
    v("ZCARD", RD),
    v("ZCOUNT", RD),
    v("ZINCRBY", WR),
    v("ZPOPMIN", WR),
    v("ZPOPMIN.BELOW", WR),
    v("ZRANGE", RD),
    v("ZRANGEBYSCORE", RD),
    v("ZRANK", RD),
    v("ZREM", WR),
    v("ZREMRANGEBYRANK", WR),
    v("ZREMRANGEBYSCORE", WR),
    v("ZREVRANGE", RD),
    v("ZREVRANGEBYSCORE", RD),
    v("ZSCAN", RD),
    v("ZSCORE", RD),
];

/// Look up an uppercase verb in [`VERBS`].
///
/// ```
/// assert!(kevy_verbs::verb(b"HSET").is_some_and(|v| v.write));
/// assert!(kevy_verbs::verb(b"PING").is_none());
/// ```
pub fn verb(upper: &[u8]) -> Option<&'static Verb> {
    VERBS.binary_search_by(|v| v.name.as_bytes().cmp(upper)).ok().map(|i| &VERBS[i])
}

/// Whether an uppercase verb is a stream (`X*`) or geo (`GEO*`) verb,
/// the family [`crate::exec`] runs only with the `streams-geo` feature.
/// The answer does not depend on the feature, so a caller that shares a
/// build with one that turned it on can still keep the family out.
///
/// ```
/// assert!(kevy_verbs::is_streams_geo(b"XADD") && kevy_verbs::is_streams_geo(b"GEOPOS"));
/// assert!(!kevy_verbs::is_streams_geo(b"GET"));
/// ```
pub fn is_streams_geo(upper: &[u8]) -> bool {
    upper.first() == Some(&b'X') || upper.starts_with(b"GEO")
}
