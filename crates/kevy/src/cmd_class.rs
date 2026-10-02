//! Verb classification tables (write / growing-write / notify class),
//! split from [`crate::cmd`] under the 500-LOC house rule. Re-exported
//! by `crate::cmd` so call sites keep their `cmd::*` paths.

use kevy_rt::NotifyKind;

/// Verb-level "is this a write" classification: the shared command
/// layer's registry for every verb it runs, [`is_server_write`] for the
/// writes the server runs itself. The local dispatch fast path and the
/// runtime (the replica write gate, the AOF, replication, WATCH) read
/// this one answer, and every command that can grow `used_memory` is in
/// it, so eviction gates them all.
#[inline]
pub(crate) fn is_write_verb(cmd: &[u8]) -> bool {
    match kevy_verbs::is_write(cmd) {
        Some(write) => write,
        None => is_server_write(cmd),
    }
}

/// The writes the server runs outside the shared command layer.
fn is_server_write(cmd: &[u8]) -> bool {
    matches!(
        cmd,
        b"BITOP"
            | b"COPY"
            // EVAL/EVALSHA count as writes so the Lua wake-bridge drains
            | b"EVAL"
            | b"EVALSHA"
            | b"SDIFFSTORE"
            | b"SINTERSTORE"
            | b"SUNIONSTORE"
            | b"XINTERNAL.CATALOG"
            | b"XINTERNAL.CONSUMERSEEN"
            | b"XINTERNAL.PENDING"
            | b"ZDIFFSTORE"
            | b"ZINTERSTORE"
            | b"ZUNIONSTORE"
            // the catalog commands: recorded, and refused on a replica
            | b"IDX.CREATE"
            | b"IDX.DROP"
            | b"IDX.REBUILD"
            | b"TABLE.DECLARE"
            | b"TABLE.DROP"
            | b"TABLE.ENSURE"
            | b"TABLE.REPLACE"
            | b"VIEW.CREATE"
            | b"VIEW.DROP"
            | b"VIEW.REBUILD"
    )
}

/// Classify an uppercased verb into a keyspace-notification class. Returns
/// `None` for read-only / non-notifying commands so the runtime can
/// short-circuit; otherwise a [`NotifyKind`] the caller matches against
/// `NotificationFlags` to decide whether to actually publish.
///
/// Event name = lowercased verb (matches the Redis events.c naming
/// convention — what redis-cli's `PSUBSCRIBE __keyevent@0__:*` reports).
/// Multi-key cmds (DEL multi / MSET / FLUSHDB) get their own per-Op
/// hooks (`maybe_notify_del` / `maybe_notify_mset` / `maybe_notify_flush`
/// in `kevy-rt::exec_notify`); this table covers single-key dispatch only.
pub(crate) fn notify_class_for_verb(cmd: &[u8]) -> Option<NotifyKind> {
    Some(match cmd {
        // String — Redis class `$`.
        b"SET" | b"SETNX" | b"SETEX" | b"PSETEX" | b"GETSET" | b"GETDEL" | b"APPEND" | b"INCR"
        | b"DECR" | b"INCRBY" | b"DECRBY" | b"INCRBYFLOAT" | b"SETBIT" | b"BITFIELD"
        | b"SETRANGE" => NotifyKind::String,
        // Hash — class `h`.
        b"HSET" | b"HSETNX" | b"HMSET" | b"HDEL" | b"HINCRBY" | b"HINCRBYFLOAT" | b"HEXPIRE"
        | b"HPEXPIRE" | b"HEXPIREAT" | b"HPEXPIREAT" | b"HPERSIST" => NotifyKind::Hash,
        // List — class `l`.
        b"LPUSH" | b"RPUSH" | b"LPUSHX" | b"RPUSHX" | b"LPOP" | b"RPOP" | b"LSET" | b"LREM"
        | b"LTRIM" | b"LINSERT" | b"RPOPLPUSH" | b"LMOVE" | b"SORT" => NotifyKind::List,
        // Set — class `s` (SINTERSTORE/SUNIONSTORE/SDIFFSTORE not yet impl'd).
        b"SADD" | b"SREM" | b"SMOVE" | b"SPOP" | b"SINTERSTORE" | b"SUNIONSTORE"
        | b"SDIFFSTORE" => NotifyKind::Set,
        // Sorted set — class `z`. GEOADD writes a ZSet under the hood,
        // so it fires `zadd` notifications too (matches Redis).
        b"ZADD" | b"ZREM" | b"ZINCRBY" | b"ZPOPMIN" | b"ZPOPMAX" | b"ZPOPMIN.BELOW"
        | b"ZREMRANGEBYRANK" | b"ZREMRANGEBYSCORE" | b"ZREMRANGEBYLEX" | b"ZRANGESTORE"
        | b"ZINTERSTORE" | b"ZUNIONSTORE" | b"ZDIFFSTORE" | b"GEOADD" => NotifyKind::Zset,
        // Stream — class `t`. XADD/XDEL/XTRIM/XGROUP/XACK/XCLAIM/
        // XREADGROUP all fire their lowercased verb name.
        b"XADD" | b"XDEL" | b"XTRIM" | b"XSETID" | b"XGROUP" | b"XACK" | b"XCLAIM"
        | b"XAUTOCLAIM" | b"XREADGROUP" => NotifyKind::Stream,
        // Generic — class `g`. (DEL single-key falls here; multi-key DEL
        // is routed through Op::Del + maybe_notify_del directly.)
        b"DEL" | b"UNLINK" | b"EXPIRE" | b"PEXPIRE" | b"PERSIST" => NotifyKind::Generic,
        // BITOP is in the same position: Redis fires `set` on the
        // destination, and a table keyed off the verb would emit
        // `bitop`.
        //
        // COPY has no arm here for the same reason as GETEX below:
        // Redis fires `copy_to` on the destination, and this table
        // publishes the lowercased verb, so an arm would emit `copy` —
        // a name Redis does not have.
        //
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
// LOC-WAIVER: data-driven verb list (one matches! arm per growing verb) —
// the same waiver its sibling `is_write_verb` carries; this list only
// crossed fifty when BITOP and COPY joined it.
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
            | b"BITFIELD"
            | b"SETRANGE"
            | b"COPY"
            | b"BITOP"
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
            | b"BLMOVE"
            | b"ZRANGESTORE"
            | b"SORT"
            | b"SMOVE"
            | b"LPUSHX"
            | b"RPUSHX"
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
            | b"MSETNX"
    )
}
