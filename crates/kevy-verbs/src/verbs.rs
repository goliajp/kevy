//! The table of verbs [`crate::exec`] answers.

/// One verb the shared layer executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Every verb [`crate::exec`] answers, sorted by name.
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
