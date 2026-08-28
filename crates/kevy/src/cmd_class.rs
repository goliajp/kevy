//! Verb classification tables (write / growing-write / notify class),
//! split from [`crate::cmd`] under the 500-LOC house rule. Re-exported
//! by `crate::cmd` so call sites keep their `cmd::*` paths.

use kevy_rt::NotifyClass;

/// Verb-level "is this a write" classification. Mirrors the `is_write` arm in
/// [`crate::KevyCommands::resolve`] so the local dispatch fast path and the
/// runtime see the same set; both must include every command that can grow
/// `used_memory`, so eviction gates them all. Kept in a single place to avoid
/// drift.
// >50-LOC exemption: pure data-driven verb match table (no control flow).
// LOC-WAIVER: data-driven verb list (one matches! arm per write verb).
pub(crate) fn is_write_verb(cmd: &[u8]) -> bool {
    matches!(
        cmd,
        b"SET"
            | b"SETNX"
            | b"SETEX"
            | b"PSETEX"
            | b"GETSET"
            | b"GETDEL"
            | b"INCRBYFLOAT"
            | b"DEL"
            | b"UNLINK"
            | b"INCR"
            | b"DECR"
            | b"INCRBY"
            | b"DECRBY"
            | b"APPEND"
            | b"SETBIT"
            | b"SETRANGE"
            | b"GETEX"
            | b"EXPIRE"
            | b"PEXPIRE"
            | b"EXPIREAT"
            | b"PEXPIREAT"
            | b"HEXPIRE"
            | b"HPEXPIRE"
            | b"HPEXPIREAT"
            | b"HPERSIST"
            | b"PERSIST"
            | b"FLUSHDB"
            | b"FLUSHALL"
            | b"HSET"
            | b"HSETNX"
            | b"HMSET"
            | b"HDEL"
            | b"HINCRBY"
            | b"HINCRBYFLOAT"
            | b"LINSERT"
            | b"LPUSH"
            | b"RPUSH"
            | b"LPOP"
            | b"RPOP"
            | b"LSET"
            | b"LREM"
            | b"LTRIM"
            | b"RPOPLPUSH"
            | b"BRPOPLPUSH"
            | b"LMOVE"
            | b"SADD"
            | b"SREM"
            | b"SPOP"
            | b"ZADD"
            | b"ZREM"
            | b"ZINCRBY"
            | b"ZPOPMIN"
            | b"ZPOPMIN.BELOW"
            | b"BZPOPMIN"
            | b"ZREMRANGEBYRANK"
            | b"ZREMRANGEBYSCORE"
            | b"ZINTERSTORE"
            | b"ZUNIONSTORE"
            | b"ZDIFFSTORE"
            | b"SINTERSTORE"
            | b"SUNIONSTORE"
            | b"SDIFFSTORE"
            | b"GEOADD"
            | b"GEOSEARCHSTORE"
            | b"GEORADIUS"
            | b"GEORADIUSBYMEMBER"
            | b"XADD"
            | b"XDEL"
            | b"XTRIM"
            | b"XSETID"
            | b"XGROUP"
            | b"XREADGROUP"
            | b"XACK"
            | b"XCLAIM"
            | b"XAUTOCLAIM"
            | b"MSET"
            // EVAL/EVALSHA count as writes so the Lua wake-bridge drains.
            | b"EVAL" | b"EVALSHA"
    )
}

/// Classify an uppercased verb into a keyspace-notification class. Returns
/// `None` for read-only / non-notifying commands so the runtime can
/// short-circuit; otherwise a [`NotifyClass`] the caller matches against
/// `NotificationFlags` to decide whether to actually publish.
///
/// Event name = lowercased verb (matches the Redis events.c naming
/// convention — what redis-cli's `PSUBSCRIBE __keyevent@0__:*` reports).
/// Multi-key cmds (DEL multi / MSET / FLUSHDB) get their own per-Op
/// hooks (`maybe_notify_del` / `maybe_notify_mset` / `maybe_notify_flush`
/// in `kevy-rt::exec_notify`); this table covers single-key dispatch only.
pub(crate) fn notify_class_for_verb(cmd: &[u8]) -> Option<NotifyClass> {
    Some(match cmd {
        // String — Redis class `$`.
        b"SET" | b"SETNX" | b"SETEX" | b"PSETEX" | b"GETSET" | b"GETDEL"
        | b"APPEND" | b"INCR" | b"DECR" | b"INCRBY" | b"DECRBY" | b"INCRBYFLOAT"
        | b"SETBIT" | b"SETRANGE" => {
            NotifyClass::String
        }
        // Hash — class `h`.
        b"HSET" | b"HSETNX" | b"HMSET" | b"HDEL" | b"HINCRBY" | b"HINCRBYFLOAT" | b"HEXPIRE"
        | b"HPEXPIRE" | b"HPEXPIREAT" | b"HPERSIST" => NotifyClass::Hash,
        // List — class `l`.
        b"LPUSH" | b"RPUSH" | b"LPOP" | b"RPOP" | b"LSET" | b"LREM" | b"LTRIM" | b"LINSERT"
        | b"RPOPLPUSH" | b"LMOVE" => NotifyClass::List,
        // Set — class `s` (SINTERSTORE/SUNIONSTORE/SDIFFSTORE not yet impl'd).
        b"SADD" | b"SREM" | b"SPOP" | b"SINTERSTORE" | b"SUNIONSTORE"
        | b"SDIFFSTORE" => NotifyClass::Set,
        // Sorted set — class `z`. GEOADD writes a ZSet under the hood,
        // so it fires `zadd` notifications too (matches Redis).
        b"ZADD" | b"ZREM" | b"ZINCRBY" | b"ZPOPMIN" | b"ZPOPMIN.BELOW" | b"ZREMRANGEBYRANK"
        | b"ZREMRANGEBYSCORE" | b"ZINTERSTORE" | b"ZUNIONSTORE" | b"ZDIFFSTORE"
        | b"GEOADD" => NotifyClass::Zset,
        // Stream — class `t`. XADD/XDEL/XTRIM/XGROUP/XACK/XCLAIM/
        // XREADGROUP all fire their lowercased verb name.
        b"XADD" | b"XDEL" | b"XTRIM" | b"XSETID" | b"XGROUP" | b"XACK" | b"XCLAIM"
        | b"XAUTOCLAIM" | b"XREADGROUP" => NotifyClass::Stream,
        // Generic — class `g`. (DEL single-key falls here; multi-key DEL
        // is routed through Op::Del + maybe_notify_del directly.)
        b"DEL" | b"UNLINK" | b"EXPIRE" | b"PEXPIRE" | b"PERSIST" => NotifyClass::Generic,
        // GETEX is a write (it can move a deadline) but has no arm
        // here on purpose: Redis fires `expire` for the EX/PX form and
        // nothing for the bare one, and this table keys the event name
        // off the verb. A `getex` event is not a name Redis ever emits,
        // so the honest choice is to emit none rather than invent one.
        //
        // Reads, admin, pub/sub etc. — no notification.
        _ => return None,
    })
}

/// Subset of [`is_write_verb`] that can *grow* memory. `DEL` / `HDEL` / `LPOP`
/// / `LREM` / `LTRIM` / `SREM` / `ZREM` / `EXPIRE` / `PERSIST` are writes but
/// only ever shrink (or hold steady), so they never need the OOM precheck —
/// and `FLUSH*` actively rescues us from OOM. Keeping them out of the precheck
/// list lets a NoEviction-configured shard always accept shrinkers, matching
/// Redis exactly.
// >50-LOC exemption: pure data-driven verb match table (no control flow).
pub(crate) fn is_growing_write_verb(cmd: &[u8]) -> bool {
    matches!(
        cmd,
        b"SET"
            | b"SETNX"
            | b"SETEX"
            | b"PSETEX"
            | b"GETSET"
            | b"INCRBYFLOAT"
            | b"INCR"
            | b"DECR"
            | b"INCRBY"
            | b"DECRBY"
            | b"APPEND"
            | b"SETBIT"
            | b"SETRANGE"
            | b"HSET"
            | b"HSETNX"
            | b"HMSET"
            | b"HINCRBY"
            | b"HINCRBYFLOAT"
            | b"LINSERT"
            | b"LPUSH"
            | b"RPUSH"
            | b"RPOPLPUSH"
            | b"BRPOPLPUSH"
            | b"LMOVE"
            | b"LSET"
            | b"SADD"
            | b"ZADD"
            | b"ZINCRBY"
            | b"ZINTERSTORE"
            | b"ZUNIONSTORE"
            | b"ZDIFFSTORE"
            | b"SINTERSTORE"
            | b"SUNIONSTORE"
            | b"SDIFFSTORE"
            | b"GEOADD"
            | b"GEOSEARCHSTORE"
            | b"GEORADIUS"
            | b"GEORADIUSBYMEMBER"
            | b"XADD"
            | b"XGROUP"
            | b"XREADGROUP"
            | b"XCLAIM"
            | b"XAUTOCLAIM"
            | b"MSET"
    )
}
