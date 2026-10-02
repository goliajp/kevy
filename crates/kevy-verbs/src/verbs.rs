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
    ///
    /// ```
    /// // lookup takes the uppercase verb and hands back its row
    /// let row = kevy_verbs::verb(b"HSET").ok_or("not in the table")?;
    /// assert_eq!(row.name, "HSET");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub name: &'static str,
    /// Whether it can change the keyspace.
    ///
    /// ```
    /// let is_write = |v: &[u8]| kevy_verbs::verb(v).map(|row| row.write);
    /// assert_eq!(is_write(b"DEL"), Some(true));
    /// assert_eq!(is_write(b"EXISTS"), Some(false));
    /// ```
    pub write: bool,
}

const fn v(name: &'static str, write: bool) -> Verb {
    Verb { name, write }
}

const RD: bool = false;
const WR: bool = true;

/// The rows below become both [`VERBS`] and [`is_write`], so the table a
/// caller reads and the lookup it matches on cannot disagree.
macro_rules! registry {
    ($($(#[$cfg:meta])* $name:literal $write:ident,)*) => {
        /// Every verb [`crate::exec`] answers, sorted by name. The stream and geo
        /// rows are there only with the `streams-geo` feature.
        ///
        /// ```
        /// let names: Vec<&str> = kevy_verbs::VERBS.iter().map(|v| v.name).collect();
        /// assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted, so lookup can bisect");
        /// assert!(kevy_verbs::VERBS.iter().any(|v| v.name == "LPUSH" && v.write));
        /// ```
        pub const VERBS: &[Verb] = &[$($(#[$cfg])* v(name($name), $write),)*];

        /// Whether an uppercase verb in [`VERBS`] can change the keyspace;
        /// `None` for a verb the table does not hold. One `match` over the
        /// same rows, for a caller that asks on every command.
        ///
        /// ```
        /// assert_eq!(kevy_verbs::is_write(b"BLPOP"), Some(true));
        /// assert_eq!(kevy_verbs::is_write(b"LRANGE"), Some(false));
        /// assert_eq!(kevy_verbs::is_write(b"PING"), None);
        /// ```
        pub fn is_write(upper: &[u8]) -> Option<bool> {
            match upper {
                $($(#[$cfg])* $name => Some($write),)*
                _ => None,
            }
        }
    };
}

// only ever evaluated while building VERBS, so a bad row fails the build
#[expect(clippy::panic, reason = "evaluated at compile time only")]
const fn name(bytes: &'static [u8]) -> &'static str {
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => panic!("a verb name is ASCII"),
    }
}

// LOC-WAIVER: pure data table — one row per shared verb.
registry! {
    b"APPEND" WR,
    b"BITCOUNT" RD,
    b"BITFIELD" WR,
    b"BITFIELD_RO" RD,
    b"BITPOS" RD,
    b"BLMOVE" WR,
    b"BLMPOP" WR,
    b"BLPOP" WR,
    b"BRPOP" WR,
    b"BRPOPLPUSH" WR,
    b"BZMPOP" WR,
    b"BZPOPMAX" WR,
    b"BZPOPMIN" WR,
    b"DBSIZE" RD,
    b"DECR" WR,
    b"DECRBY" WR,
    b"DEL" WR,
    b"DIGEST" RD,
    b"EXISTS" RD,
    b"EXPIRE" WR,
    b"EXPIREAT" WR,
    b"EXPIRETIME" RD,
    b"FLUSHALL" WR,
    b"FLUSHDB" WR,
    #[cfg(feature = "streams-geo")]
    b"GEOADD" WR,
    #[cfg(feature = "streams-geo")]
    b"GEODIST" RD,
    #[cfg(feature = "streams-geo")]
    b"GEOHASH" RD,
    #[cfg(feature = "streams-geo")]
    b"GEOPOS" RD,
    #[cfg(feature = "streams-geo")]
    b"GEORADIUS" WR,
    #[cfg(feature = "streams-geo")]
    b"GEORADIUSBYMEMBER" WR,
    #[cfg(feature = "streams-geo")]
    b"GEOSEARCH" RD,
    #[cfg(feature = "streams-geo")]
    b"GEOSEARCHSTORE" WR,
    b"GET" RD,
    b"GETBIT" RD,
    b"GETDEL" WR,
    b"GETEX" WR,
    b"GETRANGE" RD,
    b"GETSET" WR,
    b"HDEL" WR,
    b"HEXISTS" RD,
    b"HEXPIRE" WR,
    b"HEXPIREAT" WR,
    b"HEXPIRETIME" RD,
    b"HGET" RD,
    b"HGETALL" RD,
    b"HINCRBY" WR,
    b"HINCRBYFLOAT" WR,
    b"HKEYS" RD,
    b"HLEN" RD,
    b"HMGET" RD,
    b"HMSET" WR,
    b"HPERSIST" WR,
    b"HPEXPIRE" WR,
    b"HPEXPIREAT" WR,
    b"HPEXPIRETIME" RD,
    b"HPTTL" RD,
    b"HRANDFIELD" RD,
    b"HSCAN" RD,
    b"HSET" WR,
    b"HSETNX" WR,
    b"HSTRLEN" RD,
    b"HTTL" RD,
    b"HVALS" RD,
    b"INCR" WR,
    b"INCRBY" WR,
    b"INCRBYFLOAT" WR,
    b"LCS" RD,
    b"LINDEX" RD,
    b"LINSERT" WR,
    b"LLEN" RD,
    b"LMOVE" WR,
    b"LMPOP" WR,
    b"LPOP" WR,
    b"LPOS" RD,
    b"LPUSH" WR,
    b"LPUSHX" WR,
    b"LRANGE" RD,
    b"LREM" WR,
    b"LSET" WR,
    b"LTRIM" WR,
    b"MSET" WR,
    b"MSETNX" WR,
    b"PERSIST" WR,
    b"PEXPIRE" WR,
    b"PEXPIREAT" WR,
    b"PEXPIRETIME" RD,
    b"PFADD" WR,
    b"PFCOUNT" RD,
    b"PFMERGE" WR,
    b"PSETEX" WR,
    b"PTTL" RD,
    b"RENAME" WR,
    b"RENAMENX" WR,
    b"RPOP" WR,
    b"RPOPLPUSH" WR,
    b"RPUSH" WR,
    b"RPUSHX" WR,
    b"SADD" WR,
    b"SCARD" RD,
    b"SET" WR,
    b"SETBIT" WR,
    b"SETEX" WR,
    b"SETNX" WR,
    b"SETRANGE" WR,
    b"SINTERCARD" RD,
    b"SISMEMBER" RD,
    b"SMEMBERS" RD,
    b"SMISMEMBER" RD,
    b"SMOVE" WR,
    b"SORT" WR,
    b"SORT_RO" RD,
    b"SPOP" WR,
    b"SRANDMEMBER" RD,
    b"SREM" WR,
    b"SSCAN" RD,
    b"STRLEN" RD,
    b"SUBSTR" RD,
    b"TOUCH" RD,
    b"TTL" RD,
    b"TYPE" RD,
    b"UNLINK" WR,
    #[cfg(feature = "streams-geo")]
    b"XACK" WR,
    #[cfg(feature = "streams-geo")]
    b"XADD" WR,
    #[cfg(feature = "streams-geo")]
    b"XAUTOCLAIM" WR,
    #[cfg(feature = "streams-geo")]
    b"XCLAIM" WR,
    #[cfg(feature = "streams-geo")]
    b"XDEL" WR,
    #[cfg(feature = "streams-geo")]
    b"XGROUP" WR,
    #[cfg(feature = "streams-geo")]
    b"XINFO" RD,
    #[cfg(feature = "streams-geo")]
    b"XLEN" RD,
    #[cfg(feature = "streams-geo")]
    b"XPENDING" RD,
    #[cfg(feature = "streams-geo")]
    b"XRANGE" RD,
    #[cfg(feature = "streams-geo")]
    b"XREAD" RD,
    #[cfg(feature = "streams-geo")]
    b"XREADGROUP" WR,
    #[cfg(feature = "streams-geo")]
    b"XREVRANGE" RD,
    #[cfg(feature = "streams-geo")]
    b"XSETID" WR,
    #[cfg(feature = "streams-geo")]
    b"XTRIM" WR,
    b"ZADD" WR,
    b"ZCARD" RD,
    b"ZCOUNT" RD,
    b"ZDIFF" RD,
    b"ZINCRBY" WR,
    b"ZINTER" RD,
    b"ZLEXCOUNT" RD,
    b"ZMPOP" WR,
    b"ZMSCORE" RD,
    b"ZPOPMAX" WR,
    b"ZPOPMIN" WR,
    b"ZPOPMIN.BELOW" WR,
    b"ZRANDMEMBER" RD,
    b"ZRANGE" RD,
    b"ZRANGEBYLEX" RD,
    b"ZRANGEBYSCORE" RD,
    b"ZRANGESTORE" WR,
    b"ZRANK" RD,
    b"ZREM" WR,
    b"ZREMRANGEBYLEX" WR,
    b"ZREMRANGEBYRANK" WR,
    b"ZREMRANGEBYSCORE" WR,
    b"ZREVRANGE" RD,
    b"ZREVRANGEBYLEX" RD,
    b"ZREVRANGEBYSCORE" RD,
    b"ZREVRANK" RD,
    b"ZSCAN" RD,
    b"ZSCORE" RD,
    b"ZUNION" RD,
}

/// Look up an uppercase verb in [`VERBS`].
///
/// ```
/// assert!(kevy_verbs::verb(b"HSET").is_some_and(|v| v.write));
/// assert!(kevy_verbs::verb(b"PING").is_none());
/// ```
pub fn verb(upper: &[u8]) -> Option<&'static Verb> {
    VERBS.binary_search_by(|v| v.name.as_bytes().cmp(upper)).ok().map(|i| &VERBS[i])
}

/// Whether a logged frame of an uppercase verb is applied on replay: the
/// writes, and PFCOUNT — a read that writes its key's cached estimate,
/// which Redis propagates.
///
/// ```
/// assert!(kevy_verbs::replayed(b"PFADD"));
/// assert!(kevy_verbs::replayed(b"PFCOUNT"));
/// assert!(!kevy_verbs::replayed(b"GET"));
/// ```
pub fn replayed(upper: &[u8]) -> bool {
    upper == b"PFCOUNT" || verb(upper).is_some_and(|v| v.write)
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_verb_name_is_its_bytes_as_text() {
        assert_eq!(super::name(b"ZADD"), "ZADD");
    }

    #[test]
    #[should_panic(expected = "a verb name is ASCII")]
    fn a_verb_name_that_is_not_utf8_is_refused() {
        super::name(b"\xffGET");
    }
}
