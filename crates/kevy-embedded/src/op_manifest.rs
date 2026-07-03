//! ESTORE parity manifest (v2.1): command names the embedded `Store`
//! facade implements, scoped to ops present in
//! `kevy_resp::ops_table::OP_TABLE` (conn/pubsub/admin surfaces like
//! `publish` / `info` / `ping_ns` are out of the table's scope).
//! Cross-checked in `store_tests_op_table.rs` — adding a facade op
//! means updating BOTH this list and the table row's `ESTORE` bit.

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ESTORE_OPS: &[&str] = &[
    // strings
    "APPEND", "DECR", "DECRBY", "GET", "GETDEL", "GETEX", "GETRANGE",
    "GETSET", "INCR", "INCRBY", "INCRBYFLOAT", "MGET", "MSET", "SET",
    "SETNX", "SETRANGE", "STRLEN",
    // bitmap
    "BITCOUNT", "BITOP", "BITPOS", "GETBIT", "SETBIT",
    // hashes
    "HDEL", "HEXISTS", "HGET", "HGETALL", "HINCRBY", "HINCRBYFLOAT",
    "HKEYS", "HLEN", "HMGET", "HSCAN", "HSET", "HSETNX", "HVALS",
    // lists
    "LINDEX", "LINSERT", "LLEN", "LPOP", "LPUSH", "LRANGE", "LREM",
    "LSET", "LTRIM", "RPOP", "RPUSH",
    // sets
    "SADD", "SCARD", "SDIFF", "SDIFFSTORE", "SINTER", "SINTERSTORE",
    "SISMEMBER", "SMEMBERS", "SPOP", "SRANDMEMBER", "SREM", "SUNION",
    "SUNIONSTORE",
    // zsets
    "ZADD", "ZCARD", "ZCOUNT", "ZDIFFSTORE", "ZINCRBY", "ZINTERCARD",
    "ZINTERSTORE", "ZPOPMIN", "ZRANGE",
    "ZRANGEBYSCORE", "ZRANK", "ZREM", "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE", "ZREVRANGE", "ZREVRANGEBYSCORE", "ZSCAN",
    "ZSCORE", "ZUNIONSTORE",
    // keyspace
    "COPY", "DBSIZE", "DEL", "EXISTS", "EXPIRE", "EXPIREAT",
    "FLUSHALL", "KEYS", "PERSIST", "PEXPIRE", "PEXPIREAT", "RANDOMKEY",
    "RENAME", "RENAMENX", "SCAN", "TIME", "TOUCH", "TTL", "TYPE",
    "UNLINK",
];
