//! Verb documentation metadata — the single source of truth for
//! COMMAND DOCS, llms.txt, and the MCP schema.
//! Parity tests in tests_verb_meta.rs hold this table and
//! the dispatch surface bidirectionally equal.
//!
//! Flag discipline: `write` / `readonly` mirror
//! `kevy_resp::ops_table::OP_TABLE`'s `write` column **literally** for
//! every verb that has a row there (including its documented quirks:
//! RENAME/RENAMENX classify as reads because the server routes them at
//! the runtime Op level, BLPOP/BRPOP classify as reads because the
//! blocked-serve path logs the LPOP/RPOP effect, and IDX./VIEW. catalog
//! mutations are sidecar-persisted — not data writes). Verbs without an
//! OP_TABLE row (connection/admin/tx/pubsub/script/_RO twins) are
//! classified here directly.
//!
//! Arity is Redis semantics: positive = exact argc including the verb
//! itself; negative = at least |n|. Values are derived from each
//! handler's own argc check, not from Redis docs.

/// One verb's documentation row (semantic classification lives in
/// kevy_resp::ops_table::OP_TABLE — this table is the DOC face).
#[derive(Debug, Clone, Copy)]
pub struct VerbMeta {
    pub name: &'static str,
    pub group: &'static str,
    pub arity: i8,
    pub flags: &'static [&'static str],
    pub summary: &'static str,
    pub since: &'static str,
    pub syntax: &'static str,
}

const fn v(
    name: &'static str,
    group: &'static str,
    arity: i8,
    flags: &'static [&'static str],
    summary: &'static str,
    since: &'static str,
    syntax: &'static str,
) -> VerbMeta {
    VerbMeta { name, group, arity, flags, summary, since, syntax }
}

const W: &[&str] = &["write"];
const R: &[&str] = &["readonly"];
const WB: &[&str] = &["write", "blocking"];
const RB: &[&str] = &["readonly", "blocking"];
const AD: &[&str] = &["readonly", "admin"]; // introspection/control, no keyspace write
const WAD: &[&str] = &["write", "admin"]; // admin verbs with durable side effects
const TX: &[&str] = &["readonly", "transaction"];
const PS: &[&str] = &["readonly", "pubsub"];
const RX: &[&str] = &["readonly", "extension"];
const RBX: &[&str] = &["readonly", "blocking", "extension"];
const WX: &[&str] = &["write", "extension"];
const WAX: &[&str] = &["write", "admin", "extension"];

/// The registry. One row per dispatch-reachable server verb. Kept
/// grouped by family and alphabetical inside each group (OP_TABLE
/// style) so a missing row is easy to spot.
#[rustfmt::skip]
pub const VERB_META: &[VerbMeta] = &[
    // ---- connection ----------------------------------------------------
    v("ECHO",        "connection", 2,  R, "Return the given message.", "1.0.0", "ECHO message"),
    v("HELLO",       "connection", -1, R, "Handshake: report server info and switch RESP protocol version (2 or 3).", "1.0.0", "HELLO [protover]"),
    v("PING",        "connection", -1, R, "Ping the server; returns PONG or echoes the optional message.", "1.0.0", "PING [message]"),
    v("QUIT",        "connection", 1,  R, "Close the connection after replying OK.", "1.0.0", "QUIT"),
    v("SELECT",      "connection", 2,  R, "Select the logical database; kevy is single-DB and only accepts index 0.", "1.0.0", "SELECT index"),
    // ---- server --------------------------------------------------------
    v("BGREWRITEAOF", "server", 1,  WAD, "Start an append-only-file rewrite in the background.", "1.0.0", "BGREWRITEAOF"),
    v("BGSAVE",      "server", 1,  WAD, "Start a snapshot save in the background.", "1.0.0", "BGSAVE"),
    v("COMMAND",     "server", -1, R,  "Introspect the command table (COUNT, LIST, INFO, DOCS subcommands).", "1.0.0", "COMMAND [COUNT | LIST | INFO command [command ...] | DOCS [command [command ...]]]"),
    v("CONFIG",      "server", -2, WAD, "Read or change server configuration parameters.", "1.0.0", "CONFIG GET parameter | SET parameter value | REWRITE | RESETSTAT"),
    v("CLIENT",      "server", -2, AD, "Connection introspection and control subcommands.", "1.0.0", "CLIENT ID | GETNAME | SETNAME name | LIST | KILL filter | INFO | NO-EVICT ON|OFF"),
    v("CLUSTER",     "server", -2, AD, "Cluster topology introspection (virtual-node model over shards).", "1.0.0", "CLUSTER INFO | NODES | SLOTS | SHARDS | KEYSLOT key | COUNTKEYSINSLOT slot"),
    v("DBSIZE",      "server", 1,  R,  "Return the number of keys across all shards.", "1.0.0", "DBSIZE"),
    v("DEBUG",       "server", -2, AD, "Debug subcommands; SLEEP blocks the shard, other subcommands are tolerated as OK.", "1.0.0", "DEBUG SLEEP seconds | subcommand [args ...]"),
    v("FLUSHALL",    "server", -1, W,  "Delete every key on every shard.", "1.0.0", "FLUSHALL"),
    v("FLUSHDB",     "server", -1, W,  "Delete every key of the current database (kevy is single-DB: same as FLUSHALL).", "1.0.0", "FLUSHDB"),
    v("INFO",        "server", -1, R,  "Report server statistics and status sections as text.", "1.0.0", "INFO [section]"),
    v("MEMORY",      "server", -2, R,  "Memory introspection subcommands.", "1.0.0", "MEMORY USAGE key [SAMPLES count] | STATS | DOCTOR | PURGE | MALLOC-STATS"),
    v("SAVE",        "server", 1,  WAD, "Synchronously snapshot the dataset to disk.", "1.0.0", "SAVE"),
    v("SHUTDOWN",    "server", -1, WAD, "Stop the server process (connection drops without a reply).", "1.0.0", "SHUTDOWN [NOSAVE|SAVE]"),
    v("SLOWLOG",     "server", -2, AD, "Inspect or reset the slow-command log.", "1.0.0", "SLOWLOG GET [count] | LEN | RESET | HELP"),
    // ---- replication ---------------------------------------------------
    v("FAILOVER",    "replication", -3, WAD, "Planned zero-loss handover: quiesce writes, wait for the target replica to drain, promote it, and follow it.", "3.0.0", "FAILOVER host port [TIMEOUT ms] | ABORT"),
    v("REPL.TOKEN",  "replication", 1, RX, "kevy extension: mint a read-your-writes token — per-shard [generation, offset] pairs (a primary reports its live feed tail; a replica its applied positions).", "3.0.0", "REPL.TOKEN"),
    v("REPL.WAIT",   "replication", -3, RBX, "kevy extension: on a replica, block until every shard has applied the given REPL.TOKEN (then reads are read-your-writes); +OK on success, -MISDIRECTED writer is <primary> on timeout or generation mismatch. Default TIMEOUT 1000 ms, hard cap 60 s. On a primary: immediate +OK.", "3.0.0", "REPL.WAIT gen offset [gen offset ...] [TIMEOUT milliseconds]"),
    v("REPLICAOF",   "replication", 3, WAD, "Make this server a replica of another, or promote it with NO ONE.", "1.0.0", "REPLICAOF host port | NO ONE"),
    v("ROLE",        "replication", 1, R,  "Report the replication role and state of this server.", "1.0.0", "ROLE"),
    v("SLAVEOF",     "replication", 3, WAD, "Legacy alias of REPLICAOF.", "1.0.0", "SLAVEOF host port | NO ONE"),
    v("WAIT",        "replication", 3, RB, "Block until every shard's master_repl_offset is acknowledged by at least numreplicas replicas (or timeout ms pass); returns the minimum acked-replica count across shards. timeout 0 = wait forever, hard-capped at 60 s. NOTE: a replica ACK is not fsync durability.", "1.0.0", "WAIT numreplicas timeout"),
    // ---- scan (keyspace iteration) --------------------------------------
    v("KEYS",        "scan", 2,  R, "Return every key matching the glob pattern (cross-shard gather).", "1.0.0", "KEYS pattern"),
    v("RANDOMKEY",   "scan", 1,  R, "Return a random key from the keyspace.", "1.0.0", "RANDOMKEY"),
    v("SCAN",        "scan", -2, R, "Sweep the keyspace and return every match at once; NOT a cursor iterator (COUNT and TYPE are accepted and ignored, and the cursor is always 0).", "1.0.0", "SCAN cursor [MATCH pattern] [COUNT count]"),
    // ---- generic (type-agnostic key ops) --------------------------------
    v("DEL",         "generic", -2, W, "Delete one or more keys.", "1.0.0", "DEL key [key ...]"),
    v("EXISTS",      "generic", -2, R, "Count how many of the given keys exist.", "1.0.0", "EXISTS key [key ...]"),
    v("EXPIRE",      "generic", 3,  W, "Set a key's TTL in seconds (non-positive TTL deletes the key).", "1.0.0", "EXPIRE key seconds"),
    v("EXPIREAT",    "generic", 3,  W, "Set a key's expiry as an absolute Unix timestamp in seconds.", "1.0.0", "EXPIREAT key unix-time-seconds"),
    v("PERSIST",     "generic", 2,  W, "Remove a key's TTL.", "1.0.0", "PERSIST key"),
    v("PEXPIRE",     "generic", 3,  W, "Set a key's TTL in milliseconds.", "1.0.0", "PEXPIRE key milliseconds"),
    v("PEXPIREAT",   "generic", 3,  W, "Set a key's expiry as an absolute Unix timestamp in milliseconds.", "1.0.0", "PEXPIREAT key unix-time-milliseconds"),
    v("PTTL",        "generic", 2,  R, "Return a key's remaining TTL in milliseconds (-1 no TTL, -2 missing).", "1.0.0", "PTTL key"),
    v("RENAME",      "generic", 3,  W, "Rename a key, overwriting the destination (write routed at the runtime Op level).", "1.0.0", "RENAME key newkey"),
    v("RENAMENX",    "generic", 3,  W, "Rename a key only if the destination does not exist (write routed at the runtime Op level).", "1.0.0", "RENAMENX key newkey"),
    v("TTL",         "generic", 2,  R, "Return a key's remaining TTL in seconds (-1 no TTL, -2 missing).", "1.0.0", "TTL key"),
    v("TYPE",        "generic", 2,  R, "Return the type of the value stored at key.", "1.0.0", "TYPE key"),
    v("UNLINK",      "generic", -2, W, "Delete one or more keys (alias of DEL in kevy's shard model).", "1.0.0", "UNLINK key [key ...]"),
    // ---- transactions ----------------------------------------------------
    v("DISCARD",     "tx", 1,  TX, "Abort the open MULTI transaction.", "1.0.0", "DISCARD"),
    v("EXEC",        "tx", 1,  TX, "Execute every command queued since MULTI.", "1.0.0", "EXEC"),
    v("MULTI",       "tx", 1,  TX, "Start a transaction; subsequent commands are queued until EXEC.", "1.0.0", "MULTI"),
    v("UNWATCH",     "tx", 1,  TX, "Forget all watched keys.", "1.0.0", "UNWATCH"),
    v("WATCH",       "tx", -2, TX, "Watch keys for optimistic locking of the next EXEC.", "1.0.0", "WATCH key [key ...]"),
    // ---- pub/sub ---------------------------------------------------------
    v("PSUBSCRIBE",  "pubsub", -2, PS, "Subscribe to channels matching the given glob patterns.", "1.0.0", "PSUBSCRIBE pattern [pattern ...]"),
    v("PUBLISH",     "pubsub", 3,  PS, "Post a message to a channel; returns the receiver count.", "1.0.0", "PUBLISH channel message"),
    v("PUNSUBSCRIBE", "pubsub", -1, PS, "Unsubscribe from patterns (all patterns when none given).", "1.0.0", "PUNSUBSCRIBE [pattern [pattern ...]]"),
    v("SUBSCRIBE",   "pubsub", -2, PS, "Subscribe to the given channels.", "1.0.0", "SUBSCRIBE channel [channel ...]"),
    v("UNSUBSCRIBE", "pubsub", -1, PS, "Unsubscribe from channels (all channels when none given).", "1.0.0", "UNSUBSCRIBE [channel [channel ...]]"),
    // ---- scripting -------------------------------------------------------
    v("EVAL",        "script", -3, W, "Run a Lua script server-side; routed to KEYS[1]'s shard when numkeys >= 1.", "1.0.0", "EVAL script numkeys [key [key ...]] [arg [arg ...]]"),
    v("EVAL_RO",     "script", -3, R, "Read-only EVAL: the script may not call write commands.", "1.0.0", "EVAL_RO script numkeys [key [key ...]] [arg [arg ...]]"),
    v("EVALSHA",     "script", -3, W, "Run a cached Lua script by its SHA1 digest.", "1.0.0", "EVALSHA sha1 numkeys [key [key ...]] [arg [arg ...]]"),
    v("EVALSHA_RO",  "script", -3, R, "Read-only EVALSHA.", "1.0.0", "EVALSHA_RO sha1 numkeys [key [key ...]] [arg [arg ...]]"),
    v("SCRIPT",      "script", -2, R, "Manage the process-wide Lua script cache.", "1.0.0", "SCRIPT LOAD script | EXISTS sha1 [sha1 ...] | FLUSH [ASYNC|SYNC]"),
    // ---- strings ---------------------------------------------------------
    v("APPEND",      "string", 3,  W, "Append bytes to a string value; returns the new length.", "1.0.0", "APPEND key value"),
    v("DECR",        "string", 2,  W, "Decrement the integer value of a key by one.", "1.0.0", "DECR key"),
    v("DECRBY",      "string", 3,  W, "Decrement the integer value of a key by the given amount.", "1.0.0", "DECRBY key decrement"),
    v("GET",         "string", 2,  R, "Return the string value of a key.", "1.0.0", "GET key"),
    v("GETDEL",      "string", 2,  W, "Return the string value of a key and delete it.", "1.0.0", "GETDEL key"),
    v("GETSET",      "string", 3,  W, "Set a key's string value and return its previous value.", "1.0.0", "GETSET key value"),
    v("INCR",        "string", 2,  W, "Increment the integer value of a key by one.", "1.0.0", "INCR key"),
    v("INCRBY",      "string", 3,  W, "Increment the integer value of a key by the given amount.", "1.0.0", "INCRBY key increment"),
    v("INCRBYFLOAT", "string", 3,  W, "Increment the float value of a key by the given amount.", "1.0.0", "INCRBYFLOAT key increment"),
    v("MGET",        "string", -2, R, "Return the values of multiple keys (cross-shard gather).", "1.0.0", "MGET key [key ...]"),
    v("MSET",        "string", -3, W, "Set multiple key/value pairs atomically per shard.", "1.0.0", "MSET key value [key value ...]"),
    v("PSETEX",      "string", 4,  W, "Set a key's value with a TTL in milliseconds.", "1.0.0", "PSETEX key milliseconds value"),
    v("SET",         "string", -3, W, "Set a key's string value with optional TTL and existence conditions.", "1.0.0", "SET key value [EX seconds|PX milliseconds] [NX|XX]"),
    v("SETEX",       "string", 4,  W, "Set a key's value with a TTL in seconds.", "1.0.0", "SETEX key seconds value"),
    v("SETNX",       "string", 3,  W, "Set a key's value only if it does not exist.", "1.0.0", "SETNX key value"),
    v("STRLEN",      "string", 2,  R, "Return the length of a key's string value.", "1.0.0", "STRLEN key"),
    // ---- hashes ----------------------------------------------------------
    v("HDEL",        "hash", -3, W, "Delete one or more hash fields.", "1.0.0", "HDEL key field [field ...]"),
    v("HEXISTS",     "hash", 3,  R, "Check whether a hash field exists.", "1.0.0", "HEXISTS key field"),
    v("HEXPIRE",     "hash", -6, W, "Set per-field TTLs on a hash, in seconds.", "1.0.0", "HEXPIRE key seconds [NX|XX|GT|LT] FIELDS numfields field [field ...]"),
    v("HGET",        "hash", 3,  R, "Return the value of a hash field.", "1.0.0", "HGET key field"),
    v("HGETALL",     "hash", 2,  R, "Return all fields and values of a hash.", "1.0.0", "HGETALL key"),
    v("HINCRBY",     "hash", 4,  W, "Increment the integer value of a hash field.", "1.0.0", "HINCRBY key field increment"),
    v("HKEYS",       "hash", 2,  R, "Return all field names of a hash.", "1.0.0", "HKEYS key"),
    v("HLEN",        "hash", 2,  R, "Return the number of fields in a hash.", "1.0.0", "HLEN key"),
    v("HMGET",       "hash", -3, R, "Return the values of multiple hash fields.", "1.0.0", "HMGET key field [field ...]"),
    v("HMSET",       "hash", -4, W, "Deprecated alias of HSET that replies OK.", "1.0.0", "HMSET key field value [field value ...]"),
    v("HPERSIST",    "hash", -5, W, "Remove per-field TTLs from a hash.", "1.0.0", "HPERSIST key FIELDS numfields field [field ...]"),
    v("HPEXPIRE",    "hash", -6, W, "Set per-field TTLs on a hash, in milliseconds.", "1.0.0", "HPEXPIRE key milliseconds [NX|XX|GT|LT] FIELDS numfields field [field ...]"),
    v("HPEXPIREAT",  "hash", -6, W, "Set per-field expiries as absolute Unix-millisecond deadlines.", "1.0.0", "HPEXPIREAT key unix-time-milliseconds [NX|XX|GT|LT] FIELDS numfields field [field ...]"),
    v("HSCAN",       "hash", -3, R, "Iterate a hash's fields and values (single-batch cursor).", "1.0.0", "HSCAN key cursor [MATCH pattern] [COUNT count]"),
    v("HSET",        "hash", -4, W, "Set one or more hash fields; returns the number of new fields.", "1.0.0", "HSET key field value [field value ...]"),
    v("HSETNX",      "hash", 4,  W, "Set a hash field only if it does not exist.", "1.0.0", "HSETNX key field value"),
    v("HTTL",        "hash", -5, R, "Return per-field remaining TTLs in milliseconds (-1 no TTL, -2 missing).", "1.0.0", "HTTL key FIELDS numfields field [field ...]"),
    v("HVALS",       "hash", 2,  R, "Return all values of a hash.", "1.0.0", "HVALS key"),
    // ---- lists -----------------------------------------------------------
    v("BLPOP",       "list", -3, WB, "Blocking left pop across one or more lists (effect logged as LPOP).", "1.0.0", "BLPOP key [key ...] timeout"),
    v("BRPOP",       "list", -3, WB, "Blocking right pop across one or more lists (effect logged as RPOP).", "1.0.0", "BRPOP key [key ...] timeout"),
    v("BRPOPLPUSH",  "list", 4,  WB, "Blocking RPOPLPUSH: pop the source tail and push it onto the destination head.", "1.0.0", "BRPOPLPUSH source destination timeout"),
    v("LINDEX",      "list", 3,  R,  "Return the element at the given list index.", "1.0.0", "LINDEX key index"),
    v("LLEN",        "list", 2,  R,  "Return the length of a list.", "1.0.0", "LLEN key"),
    v("LMOVE",       "list", 5,  W,  "Atomically move an element between two lists.", "1.0.0", "LMOVE source destination LEFT|RIGHT LEFT|RIGHT"),
    v("LPOP",        "list", -2, W,  "Pop one or more elements from the head of a list.", "1.0.0", "LPOP key [count]"),
    v("LPOS",        "list", -3, R,  "Return the index of matching elements in a list.", "1.0.0", "LPOS key element [RANK rank] [COUNT num-matches] [MAXLEN len]"),
    v("LPUSH",       "list", -3, W,  "Prepend one or more elements to a list.", "1.0.0", "LPUSH key element [element ...]"),
    v("LRANGE",      "list", 4,  R,  "Return a range of list elements.", "1.0.0", "LRANGE key start stop"),
    v("LREM",        "list", 4,  W,  "Remove elements equal to the given value from a list.", "1.0.0", "LREM key count element"),
    v("LSET",        "list", 4,  W,  "Set the list element at the given index.", "1.0.0", "LSET key index element"),
    v("LTRIM",       "list", 4,  W,  "Trim a list to the given range.", "1.0.0", "LTRIM key start stop"),
    v("RPOP",        "list", -2, W,  "Pop one or more elements from the tail of a list.", "1.0.0", "RPOP key [count]"),
    v("RPOPLPUSH",   "list", 3,  W,  "Pop the source tail and push it onto the destination head.", "1.0.0", "RPOPLPUSH source destination"),
    v("RPUSH",       "list", -3, W,  "Append one or more elements to a list.", "1.0.0", "RPUSH key element [element ...]"),
    // ---- sets ------------------------------------------------------------
    v("SADD",        "set", -3, W, "Add one or more members to a set.", "1.0.0", "SADD key member [member ...]"),
    v("SCARD",       "set", 2,  R, "Return the number of members in a set.", "1.0.0", "SCARD key"),
    v("SDIFF",       "set", -2, R, "Return the difference of the given sets (cross-shard gather).", "1.0.0", "SDIFF key [key ...]"),
    v("SDIFFSTORE",  "set", -3, W, "Store the difference of the given sets into destination.", "1.0.0", "SDIFFSTORE destination key [key ...]"),
    v("SINTER",      "set", -2, R, "Return the intersection of the given sets (cross-shard gather).", "1.0.0", "SINTER key [key ...]"),
    v("SINTERSTORE", "set", -3, W, "Store the intersection of the given sets into destination.", "1.0.0", "SINTERSTORE destination key [key ...]"),
    v("SISMEMBER",   "set", 3,  R, "Check whether a value is a member of a set.", "1.0.0", "SISMEMBER key member"),
    v("SMEMBERS",    "set", 2,  R, "Return all members of a set.", "1.0.0", "SMEMBERS key"),
    v("SPOP",        "set", -2, W, "Remove and return one or more random members of a set.", "1.0.0", "SPOP key [count]"),
    v("SRANDMEMBER", "set", -2, R, "Return one or more random members of a set without removing them.", "1.0.0", "SRANDMEMBER key [count]"),
    v("SREM",        "set", -3, W, "Remove one or more members from a set.", "1.0.0", "SREM key member [member ...]"),
    v("SSCAN",       "set", -3, R, "Iterate a set's members (single-batch cursor).", "1.0.0", "SSCAN key cursor [MATCH pattern] [COUNT count]"),
    v("SUNION",      "set", -2, R, "Return the union of the given sets (cross-shard gather).", "1.0.0", "SUNION key [key ...]"),
    v("SUNIONSTORE", "set", -3, W, "Store the union of the given sets into destination.", "1.0.0", "SUNIONSTORE destination key [key ...]"),
    // ---- sorted sets -----------------------------------------------------
    v("BZPOPMIN",    "zset", -3, WB, "Blocking pop of the lowest-scored member across one or more sorted sets.", "1.0.0", "BZPOPMIN key [key ...] timeout"),
    v("ZADD",        "zset", -4, W,  "Add members with scores to a sorted set, with conditional flags.", "1.0.0", "ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]"),
    v("ZCARD",       "zset", 2,  R,  "Return the number of members in a sorted set.", "1.0.0", "ZCARD key"),
    v("ZCOUNT",      "zset", 4,  R,  "Count members with scores within the given bounds.", "1.0.0", "ZCOUNT key min max"),
    v("ZDIFFSTORE",  "zset", -4, W,  "Store the difference of the given sorted sets into destination.", "1.0.0", "ZDIFFSTORE destination numkeys key [key ...]"),
    v("ZINCRBY",     "zset", 4,  W,  "Increment a member's score in a sorted set.", "1.0.0", "ZINCRBY key increment member"),
    v("ZINTERCARD",  "zset", -3, R,  "Return the cardinality of the intersection of the given sorted sets.", "1.0.0", "ZINTERCARD numkeys key [key ...] [LIMIT limit]"),
    v("ZINTERSTORE", "zset", -4, W,  "Store the intersection of the given sorted sets into destination.", "1.0.0", "ZINTERSTORE destination numkeys key [key ...] [WEIGHTS weight [weight ...]] [AGGREGATE SUM|MIN|MAX]"),
    v("ZPOPMIN",     "zset", -2, W,  "Pop up to count members with the lowest scores.", "1.0.0", "ZPOPMIN key [count]"),
    v("ZPOPMIN.BELOW", "zset", -3, &["write", "extension"], "kevy extension: pop up to count lowest members with score strictly below the threshold (delayed-job primitive).", "3.0.0", "ZPOPMIN.BELOW key below [count]"),
    v("ZRANGE",      "zset", -4, R,  "Return members by rank range.", "1.0.0", "ZRANGE key start stop [WITHSCORES]"),
    v("ZRANGEBYSCORE", "zset", -4, R, "Return members with scores within the given bounds.", "1.0.0", "ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]"),
    v("ZRANK",       "zset", 3,  R,  "Return a member's rank, ordered from the lowest score.", "1.0.0", "ZRANK key member"),
    v("ZREM",        "zset", -3, W,  "Remove one or more members from a sorted set.", "1.0.0", "ZREM key member [member ...]"),
    v("ZREMRANGEBYRANK", "zset", 4, W, "Remove members within the given rank range.", "1.0.0", "ZREMRANGEBYRANK key start stop"),
    v("ZREMRANGEBYSCORE", "zset", 4, W, "Remove members with scores within the given bounds.", "1.0.0", "ZREMRANGEBYSCORE key min max"),
    v("ZREVRANGEBYSCORE", "zset", -4, R, "Return members with scores within the given bounds, highest first.", "1.0.0", "ZREVRANGEBYSCORE key max min [WITHSCORES] [LIMIT offset count]"),
    v("ZSCAN",       "zset", -3, R,  "Iterate a sorted set's members and scores (single-batch cursor).", "1.0.0", "ZSCAN key cursor [MATCH pattern] [COUNT count]"),
    v("ZSCORE",      "zset", 3,  R,  "Return a member's score.", "1.0.0", "ZSCORE key member"),
    v("ZUNIONSTORE", "zset", -4, W,  "Store the union of the given sorted sets into destination.", "1.0.0", "ZUNIONSTORE destination numkeys key [key ...] [WEIGHTS weight [weight ...]] [AGGREGATE SUM|MIN|MAX]"),
    // ---- streams ---------------------------------------------------------
    v("XACK",        "stream", -4, W, "Acknowledge pending entries for a consumer group.", "1.0.0", "XACK key group id [id ...]"),
    v("XADD",        "stream", -5, W, "Append an entry to a stream, with optional trim.", "1.0.0", "XADD key [NOMKSTREAM] [MAXLEN [=|~] threshold | MINID [=|~] id] id|* field value [field value ...]"),
    v("XAUTOCLAIM",  "stream", -6, W, "Scan and claim idle pending entries starting from a cursor.", "1.0.0", "XAUTOCLAIM key group consumer min-idle-time start [COUNT count] [JUSTID]"),
    v("XCLAIM",      "stream", -6, W, "Claim specific idle pending entries for a consumer.", "1.0.0", "XCLAIM key group consumer min-idle-time id [id ...] [IDLE ms] [TIME unix-time-milliseconds] [RETRYCOUNT count] [FORCE] [JUSTID]"),
    v("XDEL",        "stream", -3, W, "Delete entries from a stream by ID.", "1.0.0", "XDEL key id [id ...]"),
    v("XGROUP",      "stream", -2, W, "Manage stream consumer groups.", "1.0.0", "XGROUP CREATE key group id|$ [MKSTREAM] | DESTROY key group | SETID key group id|$ | CREATECONSUMER key group consumer | DELCONSUMER key group consumer"),
    v("XINFO",       "stream", -2, R, "Introspect a stream, its groups, or its consumers.", "1.0.0", "XINFO STREAM key | GROUPS key | CONSUMERS key group | HELP"),
    v("XLEN",        "stream", 2,  R, "Return the number of entries in a stream.", "1.0.0", "XLEN key"),
    v("XPENDING",    "stream", -3, R, "Inspect pending entries of a consumer group (summary or extended form).", "1.0.0", "XPENDING key group [[IDLE min-idle-time] start end count [consumer]]"),
    v("XRANGE",      "stream", -4, R, "Return stream entries within an ID range.", "1.0.0", "XRANGE key start end [COUNT count]"),
    v("XREAD",       "stream", -4, RB, "Read new entries from one or more streams, optionally blocking.", "1.0.0", "XREAD [COUNT count] [BLOCK milliseconds] STREAMS key [key ...] id [id ...]"),
    v("XREADGROUP",  "stream", -7, WB, "Read stream entries as a consumer-group member, optionally blocking.", "1.0.0", "XREADGROUP GROUP group consumer [COUNT count] [BLOCK milliseconds] [NOACK] STREAMS key [key ...] id [id ...]"),
    v("XREVRANGE",   "stream", -4, R, "Return stream entries within an ID range, in reverse order.", "1.0.0", "XREVRANGE key end start [COUNT count]"),
    v("XSETID",      "stream", -3, W, "Overwrite a stream's last-generated ID and bookkeeping counters.", "1.0.0", "XSETID key last-id [ENTRIESADDED entries-added] [MAXDELETEDID max-deleted-id]"),
    v("XTRIM",       "stream", -4, W, "Trim a stream by length or minimum ID.", "1.0.0", "XTRIM key MAXLEN|MINID [=|~] threshold"),
    // ---- geo (zset-backed) -----------------------------------------------
    v("GEOADD",      "geo", -5, W, "Add geospatial members (longitude/latitude) to a geo set.", "1.0.0", "GEOADD key [NX|XX] [CH] longitude latitude member [longitude latitude member ...]"),
    v("GEODIST",     "geo", -4, R, "Return the distance between two geo members.", "1.0.0", "GEODIST key member1 member2 [m|km|mi|ft]"),
    v("GEOHASH",     "geo", -3, R, "Return the base32 geohash string of each member.", "1.0.0", "GEOHASH key member [member ...]"),
    v("GEOPOS",      "geo", -3, R, "Return the longitude/latitude of each member.", "1.0.0", "GEOPOS key member [member ...]"),
    v("GEORADIUS",   "geo", -6, W, "Legacy radius search around a coordinate, with optional STORE writes.", "1.0.0", "GEORADIUS key longitude latitude radius m|km|mi|ft [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC|DESC] [STORE key] [STOREDIST key]"),
    v("GEORADIUS_RO", "geo", -6, R, "Read-only GEORADIUS (STORE options rejected).", "1.0.0", "GEORADIUS_RO key longitude latitude radius m|km|mi|ft [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC|DESC]"),
    v("GEORADIUSBYMEMBER", "geo", -5, W, "Legacy radius search around an existing member, with optional STORE writes.", "1.0.0", "GEORADIUSBYMEMBER key member radius m|km|mi|ft [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC|DESC] [STORE key] [STOREDIST key]"),
    v("GEORADIUSBYMEMBER_RO", "geo", -5, R, "Read-only GEORADIUSBYMEMBER (STORE options rejected).", "1.0.0", "GEORADIUSBYMEMBER_RO key member radius m|km|mi|ft [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC|DESC]"),
    v("GEOSEARCH",   "geo", -4, R, "Search members within a radius or box from a member or coordinate.", "1.0.0", "GEOSEARCH key FROMMEMBER member|FROMLONLAT longitude latitude BYRADIUS radius m|km|mi|ft|BYBOX width height m|km|mi|ft [ASC|DESC] [COUNT count [ANY]] [WITHCOORD] [WITHDIST] [WITHHASH]"),
    v("GEOSEARCHSTORE", "geo", -5, W, "Run GEOSEARCH on source and store the hits into destination.", "1.0.0", "GEOSEARCHSTORE destination source FROMMEMBER member|FROMLONLAT longitude latitude BYRADIUS radius m|km|mi|ft|BYBOX width height m|km|mi|ft [ASC|DESC] [COUNT count [ANY]] [STOREDIST]"),
    // ---- index (IDX.* extension) -------------------------------------------
    v("IDX.COUNT",   "index", -4, RX, "Count index entries matching a range or equality predicate.", "3.0.0", "IDX.COUNT name RANGE min max | EQ value"),
    v("IDX.CREATE",  "index", -11, WX, "Declare a secondary index over a key prefix (catalog mutation, sidecar-persisted).", "3.0.0", "IDX.CREATE name ON PREFIX prefix FIELD field TYPE i64|f64|str|vector KIND range|unique|text|ann|agg [MAXMEM bytes] [DIM dim] [DISTANCE cosine|l2|ip] [M m] [EF ef] [GROUPBY field]"),
    v("IDX.DROP",    "index", 2,  WX, "Drop a declared index (catalog mutation, sidecar-persisted).", "3.0.0", "IDX.DROP name"),
    v("IDX.LIST",    "index", 1,  RX, "List declared indexes with per-index build state and stats.", "3.0.0", "IDX.LIST"),
    v("IDX.EXPLAIN", "index", -2, RX, "The IDX.QUERY parse without the execution: kind, state, est_rows, and the plan line.", "3.0.0", "IDX.EXPLAIN name RANGE min max|EQ value|MATCH text|KNN vector|GROUPS [args ...]"),
    v("IDX.QUERY",   "index", -4, RX, "Query an index: range/equality, text MATCH, vector KNN, aggregation groups, or a two-index COMPOSE.", "3.0.0", "IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c] [FIELDS field ...] | name EQ value [LIMIT n] [CURSOR c] [FIELDS field ...] | name MATCH text [LIMIT n] [FIELDS field ...] | name KNN vector [LIMIT k] [EF ef] [FIELDS field ...] | name GROUP group | name GROUPS [BY count|sum|min|max] [LIMIT n] | HYBRID text_idx MATCH text ann_idx KNN vector [LIMIT n] [RRFK k] [EF ef] [FIELDS field ...] | COMPOSE AND|OR nameA RANGE min max|EQ value nameB RANGE min max|EQ value [LIMIT n] [CURSOR c] [FIELDS field ...]"),
    v("IDX.REBUILD", "index", 2,  WX, "Rebuild an ANN index (tombstone compaction).", "3.0.0", "IDX.REBUILD name"),
    v("IDX.VERIFY",  "index", 2,  RX, "Verify an index: re-read every held entry against its row and report entries/bytes/coerce_failures/duplicates/drift/checked.", "3.0.0", "IDX.VERIFY name"),
    // ---- views (VIEW.* extension) -------------------------------------------
    v("VIEW.CREATE", "view", -8, WX, "Declare a composed view over indexes with an ORDER BY (catalog mutation, sidecar-persisted).", "3.0.0", "VIEW.CREATE name QUERY tree ORDER BY index [DESC] [MODE virtual|materialized] [TOPK k] [VIA template] (tree = '( AND|OR|DIFF sub sub )' | 'index RANGE min max' | 'index EQ value')"),
    v("VIEW.DROP",   "view", 2,  WX, "Drop a declared view (catalog mutation, sidecar-persisted).", "3.0.0", "VIEW.DROP name"),
    v("VIEW.EXPLAIN", "view", 2, RX, "Explain a view's composition tree with per-leaf cardinalities.", "3.0.0", "VIEW.EXPLAIN name"),
    v("VIEW.LIST",   "view", 1,  RX, "List declared views with their mode and shape.", "3.0.0", "VIEW.LIST"),
    v("VIEW.QUERY",  "view", -2, RX, "Page through a view's ordered members, optionally hydrating fields via the VIA template.", "3.0.0", "VIEW.QUERY name [LIMIT n] [CURSOR c] [FIELDS field ...]"),
    v("VIEW.REBUILD", "view", 2, WX, "Force a rebuild of a materialized view.", "3.0.0", "VIEW.REBUILD name"),
    v("VIEW.VERIFY", "view", 2,  RX, "Verify a view and report member/byte statistics.", "3.0.0", "VIEW.VERIFY name"),
    // ---- feed (CDC extension) ------------------------------------------------
    v("FEED.READ",   "feed", -4, RX, "Read change-feed frames from one shard past a generation/offset cursor.", "3.0.0", "FEED.READ shard generation offset [COUNT n] [PREFIX prefix ...]"),
    v("FEED.SHARDS", "feed", 1,  RX, "Return the number of change-feed shards.", "3.0.0", "FEED.SHARDS"),
    v("FEED.TAIL",   "feed", 2,  RX, "Return a shard's current feed generation and next offset.", "3.0.0", "FEED.TAIL shard"),
    v("PREFIX.STATS", "migration", 2, RX, "Report key-count and byte statistics for a key prefix.", "3.0.0", "PREFIX.STATS prefix"),
    // ---- migration -------------------------------------------------------------
    v("MOVE-SCOPE",  "migration", 6, WAX, "Operator-issued: migrate every key under a prefix from one node to another (internal cluster op).", "3.0.0", "MOVE-SCOPE prefix FROM from-node-id TO to-node-id"),
    v("MOVE-SCOPE-INGEST", "migration", 3, WAX, "Target-side receiver of a MOVE-SCOPE bulk payload (internal cluster op).", "3.0.0", "MOVE-SCOPE-INGEST prefix bulk-payload"),
    v("PREFIX.DIGEST", "migration", 2, RX, "Order-insensitive checksum of a prefix's rows for migration verification.", "3.0.0", "PREFIX.DIGEST prefix"),
];

/// Look up a row by canonical (uppercase) name.
pub fn verb_meta(name: &str) -> Option<&'static VerbMeta> {
    VERB_META.iter().find(|m| m.name == name)
}
