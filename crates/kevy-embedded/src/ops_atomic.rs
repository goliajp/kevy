//! Single-shard read-modify-write closure: `Store::atomic`.
//!
//! `atomic(|tx| { ... })` holds the shard's write lock for the
//! closure body. Reads inside the closure see prior writes inside
//! the same closure, so read-modify-write loops work as expected.
//! AOF writes are deferred and batched into a single fsync at
//! commit time.
//!
//! Every key touched inside the closure must hash to the same
//! shard. For closures that span shards use
//! [`Store::atomic_all_shards`](crate::Store::atomic_all_shards).

use crate::{KevyError, KevyResult};
use std::sync::RwLockWriteGuard;

use crate::store::ensure_writable;
use crate::store::{Inner, Store, commit_write, store_err};

/// One key's pre-transaction state: the key, and what was there before
/// the transaction first touched it (`None` = the key did not exist).
type UndoEntry = (Vec<u8>, Option<(kevy_store::Value, Option<u64>)>);

/// Handle passed to the `atomic` closure body. Methods mirror the
/// equivalent `Store` ops but operate on the already-held write
/// lock, so reads inside the block see the closure's own writes.
pub struct AtomicCtx<'a> {
    inner: &'a mut Inner,
    log: Vec<Vec<Vec<u8>>>,
    /// Prior state of every key this transaction has touched, captured
    /// on FIRST touch: `None` means the key did not exist. Replayed in
    /// reverse by [`Store::atomic`] when the closure returns `Err`, so a
    /// rejected transaction leaves neither memory nor the AOF changed.
    undo: Vec<UndoEntry>,
    /// Keys already in `undo` — a key is snapshotted once, before its
    /// first mutation, never after.
    touched: std::collections::HashSet<Vec<u8>>,
}

impl AtomicCtx<'_> {
    // ---- string ops ------------------------------------------------

    /// `SET key value`. Returns `true` (SET always succeeds without
    /// `NX`/`XX` veto).
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> bool {
        self.snap(key);
        let ok = self.inner.store.set(key, value.to_vec(), None, false, false);
        self.log_arg(&[b"SET", key, value]);
        ok
    }

    /// `GET key`.
    pub fn get(&mut self, key: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        self.inner.store.get(key).map(|opt| opt.as_deref().map(<[u8]>::to_vec)).map_err(store_err)
    }

    /// `INCR key` — by 1.
    pub fn incr(&mut self, key: &[u8]) -> KevyResult<i64> {
        self.snap(key);
        let n = self.inner.store.incr_by(key, 1).map_err(store_err)?;
        self.log_arg(&[b"INCR", key]);
        Ok(n)
    }

    /// `INCRBY key delta`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> KevyResult<i64> {
        self.snap(key);
        let n = self.inner.store.incr_by(key, delta).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(&[b"INCRBY", key, s.as_bytes()]);
        Ok(n)
    }

    // ---- hash ops ---------------------------------------------------

    /// `HSET key field value`.
    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> KevyResult<usize> {
        self.snap(key);
        let n = self.inner.store.hset(key, pairs).map_err(store_err)?;
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        parts.push(b"HSET");
        parts.push(key);
        for (f, v) in pairs {
            parts.push(f);
            parts.push(v);
        }
        self.log_arg(&parts);
        Ok(n)
    }

    /// `HGET key field`.
    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        Ok(self.inner.store.hget(key, field).map_err(store_err)?.map(<[u8]>::to_vec))
    }

    /// `HINCRBY key field delta`.
    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> KevyResult<i64> {
        self.snap(key);
        let n = self.inner.store.hincrby(key, field, delta).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(&[b"HINCRBY", key, field, s.as_bytes()]);
        Ok(n)
    }

    // ---- zset ops ---------------------------------------------------

    /// `ZADD key score member`.
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> KevyResult<usize> {
        self.snap(key);
        let n = self.inner.store.zadd(key, pairs).map_err(store_err)?;
        let score_strs: Vec<Vec<u8>> =
            pairs.iter().map(|(s, _)| format!("{s}").into_bytes()).collect();
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        parts.push(b"ZADD");
        parts.push(key);
        for (i, (_, m)) in pairs.iter().enumerate() {
            parts.push(&score_strs[i]);
            parts.push(m);
        }
        self.log_arg(&parts);
        Ok(n)
    }

    /// `ZINCRBY key delta member`.
    pub fn zincrby(&mut self, key: &[u8], delta: f64, member: &[u8]) -> KevyResult<f64> {
        self.snap(key);
        let n = self.inner.store.zincrby(key, delta, member).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(&[b"ZINCRBY", key, s.as_bytes(), member]);
        Ok(n)
    }

    /// `ZSCORE key member`.
    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> KevyResult<Option<f64>> {
        self.inner.store.zscore(key, member).map_err(store_err)
    }

    // ---- helpers ----------------------------------------------------

    // ---- keyspace ops (Pipeline write parity) ----------------------

    /// `DEL key [key ...]` — every key must hash to this shard.
    pub fn del(&mut self, keys: &[&[u8]]) -> usize {
        for k in keys {
            self.snap(k);
        }
        let n = self.inner.store.del(keys);
        if n > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(1 + keys.len());
            argv.push(b"DEL");
            argv.extend_from_slice(keys);
            self.log_arg(&argv);
        }
        n
    }

    /// `EXISTS key [key ...]` — count of the given keys that exist.
    pub fn exists(&mut self, keys: &[&[u8]]) -> usize {
        keys.iter().filter(|k| self.inner.store.key_exists(k)).count()
    }

    // ---- hash ops --------------------------------------------------

    /// `HDEL key field [field ...]`.
    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<usize> {
        self.snap(key);
        let removed = self.inner.store.hdel(key, fields).map_err(store_err)?;
        if removed > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + fields.len());
            argv.push(b"HDEL");
            argv.push(key);
            argv.extend_from_slice(fields);
            self.log_arg(&argv);
        }
        Ok(removed)
    }

    /// `HGETALL key` — `(field, value)` pairs; reads see the
    /// closure's own prior writes.
    pub fn hgetall(&mut self, key: &[u8]) -> KevyResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let flat = self.inner.store.hgetall(key).map_err(store_err)?;
        let mut out = Vec::with_capacity(flat.len() / 2);
        let mut it = flat.into_iter();
        while let (Some(f), Some(v)) = (it.next(), it.next()) {
            out.push((f, v));
        }
        Ok(out)
    }

    /// `HMGET key field [field ...]` — `None` per absent field.
    pub fn hmget(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<Option<Vec<u8>>>> {
        self.inner.store.hmget(key, fields).map_err(store_err)
    }

    /// `HEXISTS key field`.
    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> KevyResult<bool> {
        self.inner.store.hexists(key, field).map_err(store_err)
    }

    // ---- set ops ---------------------------------------------------

    /// `SADD key member [member ...]`.
    pub fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        self.snap(key);
        let added = self.inner.store.sadd(key, members).map_err(store_err)?;
        if added > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            argv.push(b"SADD");
            argv.push(key);
            argv.extend_from_slice(members);
            self.log_arg(&argv);
        }
        Ok(added)
    }

    /// `SREM key member [member ...]`.
    pub fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        self.snap(key);
        let removed = self.inner.store.srem(key, members).map_err(store_err)?;
        if removed > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            argv.push(b"SREM");
            argv.push(key);
            argv.extend_from_slice(members);
            self.log_arg(&argv);
        }
        Ok(removed)
    }

    // ---- list ops --------------------------------------------------

    /// `LPUSH key value [value ...]` — returns the new list length.
    pub fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> KevyResult<usize> {
        self.snap(key);
        let len = self.inner.store.lpush(key, values).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        argv.push(b"LPUSH");
        argv.push(key);
        argv.extend_from_slice(values);
        self.log_arg(&argv);
        Ok(len)
    }

    /// `RPUSH key value [value ...]` — returns the new list length.
    pub fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> KevyResult<usize> {
        self.snap(key);
        let len = self.inner.store.rpush(key, values).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        argv.push(b"RPUSH");
        argv.push(key);
        argv.extend_from_slice(values);
        self.log_arg(&argv);
        Ok(len)
    }

    // ---- zset ops --------------------------------------------------

    /// `ZREM key member [member ...]`.
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        self.snap(key);
        let removed = self.inner.store.zrem(key, members).map_err(store_err)?;
        if removed > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            argv.push(b"ZREM");
            argv.push(key);
            argv.extend_from_slice(members);
            self.log_arg(&argv);
        }
        Ok(removed)
    }

    /// `ZCARD key` — member count; 0 when absent.
    pub fn zcard(&mut self, key: &[u8]) -> KevyResult<usize> {
        self.inner.store.zcard(key).map_err(store_err)
    }

    /// Flags-aware `ZADD`. AOF logs the applied pairs as plain
    /// `ZADD` — the effect, never the condition (deterministic replay).
    pub fn zadd_flags(
        &mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
        flags: kevy_store::ZaddFlags,
    ) -> KevyResult<kevy_store::ZaddReport> {
        if !flags.valid() {
            return Err(KevyError::InvalidInput("invalid ZADD flag combo".into()));
        }
        let rep = self.inner.store.zadd_flags(key, pairs, flags).map_err(store_err)?;
        if !rep.applied.is_empty() {
            let score_strs: Vec<Vec<u8>> =
                rep.applied.iter().map(|(s, _)| format!("{s}").into_bytes()).collect();
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + rep.applied.len() * 2);
            parts.push(b"ZADD");
            parts.push(key);
            for (i, (_, m)) in rep.applied.iter().enumerate() {
                parts.push(&score_strs[i]);
                parts.push(m);
            }
            self.log_arg(&parts);
        }
        Ok(rep)
    }

    // ---- collection reads --------------------------------------------
    // Requested by a consumer: a set could be written inside a transaction but never read back
    // inside one, so any child collection a cascade delete must
    // enumerate had to be modelled as a hash — they reshaped a whole
    // keyspace around the omission. These hold the shard write lock
    // already, so there was never a consistency reason to withhold them.

    /// `SMEMBERS key`.
    pub fn smembers(&mut self, key: &[u8]) -> KevyResult<Vec<Vec<u8>>> {
        self.inner.store.smembers(key).map_err(store_err)
    }

    /// `SISMEMBER key member`.
    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> KevyResult<bool> {
        self.inner.store.sismember(key, member).map_err(store_err)
    }

    /// `LRANGE key start stop` (inclusive, negatives count from the end).
    pub fn lrange(&mut self, key: &[u8], start: i64, stop: i64) -> KevyResult<Vec<Vec<u8>>> {
        self.inner.store.lrange(key, start, stop).map_err(store_err)
    }

    /// `LLEN key`.
    pub fn llen(&mut self, key: &[u8]) -> KevyResult<usize> {
        self.inner.store.llen(key).map_err(store_err)
    }

    /// `SCARD key`.
    pub fn scard(&mut self, key: &[u8]) -> KevyResult<usize> {
        self.inner.store.scard(key).map_err(store_err)
    }

    /// `ZRANGEBYSCORE key min max` — `(member, score)` in score order.
    pub fn zrangebyscore(
        &mut self,
        key: &[u8],
        min: kevy_store::ScoreBound,
        max: kevy_store::ScoreBound,
    ) -> KevyResult<Vec<(Vec<u8>, f64)>> {
        self.inner.store.zrange_by_score(key, min, max).map_err(store_err)
    }

    /// Record `key`'s prior state, once, before its first mutation.
    fn snap(&mut self, key: &[u8]) {
        if self.touched.contains(key) {
            return;
        }
        let prior = self.inner.store.clone_with_ttl(key);
        self.touched.insert(key.to_vec());
        self.undo.push((key.to_vec(), prior));
    }

    fn log_arg(&mut self, parts: &[&[u8]]) {
        self.log.push(parts.iter().map(|p| p.to_vec()).collect());
    }
}

impl Store {
    /// Run `body` as a single-shard atomic transaction: it applies
    /// entirely, or not at all.
    ///
    /// Inside the closure every read sees the closure's own previous
    /// writes. On `Ok`, the queued AOF frames are committed as one
    /// group — under `Fsync::Always` that is a single fsync for the
    /// whole block, not one per mutation.
    ///
    /// On `Err`, **every write the closure made is rolled back** and
    /// nothing is appended to the AOF. This is what lets the closure
    /// act as the enforcement point for an invariant: read, decide,
    /// write, and return `Err` to reject — the rejection leaves no
    /// trace. (Before 4.0 the writes stayed live in memory while their
    /// AOF frames were discarded, so a restarted process disagreed
    /// with the running one.)
    ///
    /// Rollback restores each touched key to the value and TTL it had
    /// before the transaction — including deleting keys the closure
    /// created. It is a snapshot of the keys the closure touches, so
    /// the cost scales with the transaction, not the keyspace.
    ///
    /// Constraint: every key touched inside the closure must hash to
    /// the same shard. The default embedded config uses 1 shard, so
    /// any key works.
    pub fn atomic<R>(
        &self,
        body: impl FnOnce(&mut AtomicCtx<'_>) -> KevyResult<R>,
    ) -> KevyResult<R> {
        ensure_writable(self)?;
        let mut g: RwLockWriteGuard<'_, Inner> = self.lock();
        let mut ctx = AtomicCtx {
            inner: &mut g,
            log: Vec::new(),
            undo: Vec::new(),
            touched: std::collections::HashSet::new(),
        };
        let outcome = body(&mut ctx);
        let log = std::mem::take(&mut ctx.log);
        let undo = std::mem::take(&mut ctx.undo);
        let r = match outcome {
            Ok(r) => r,
            Err(e) => {
                rollback(&mut g, undo);
                return Err(e);
            }
        };
        commit_group(&mut g, log)?;
        Ok(r)
    }
}

/// Parity manifest: command names `AtomicCtx` implements.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ATOMIC_OPS: &[&str] = &[
    "SET",
    "GET",
    "INCR",
    "INCRBY",
    "HSET",
    "HGET",
    "HINCRBY",
    "ZADD",
    "ZINCRBY",
    "ZSCORE",
    "DEL",
    "EXISTS",
    "HDEL",
    "HGETALL",
    "HMGET",
    "HEXISTS",
    "SADD",
    "SREM",
    "LPUSH",
    "RPUSH",
    "ZREM",
    "ZCARD",
    "SMEMBERS",
    "SISMEMBER",
    "LRANGE",
    "LLEN",
    "SCARD",
    "ZRANGEBYSCORE",
];

/// Undo a rejected transaction.
///
/// The closure's writes hit the store as they were made — reads inside
/// the block have to see them — so a rejected transaction must be undone
/// here, or the rejected write stays live while its AOF frames are
/// discarded and a restart disagrees with the running process. Reverse
/// order so a key touched more than once lands on its earliest recorded
/// state.
fn rollback(g: &mut Inner, undo: Vec<UndoEntry>) {
    for (key, prior) in undo.into_iter().rev() {
        match prior {
            Some((value, ttl_ms)) => g.store.put_with_ttl(key, value, ttl_ms),
            None => {
                let k: &[u8] = &key;
                g.store.del(&[k]);
            }
        }
    }
}

/// Commit the queued AOF frames as ONE bracketed group.
///
/// The brackets are what make replay all-or-nothing at any size, and the
/// group is what makes `Fsync::Always` cost one sync instead of N. See
/// `kevy_persist::Aof::begin_group`.
fn commit_group(g: &mut Inner, log: Vec<Vec<Vec<u8>>>) -> KevyResult<()> {
    #[cfg(feature = "persist")]
    if let Some(aof) = g.aof.as_mut() {
        aof.begin_group();
    }
    let mut commit = Ok(());
    for entry in log {
        let parts: Vec<&[u8]> = entry.iter().map(|v| v.as_slice()).collect();
        commit = commit_write(g, &parts);
        if commit.is_err() {
            break;
        }
    }
    #[cfg(feature = "persist")]
    if let Some(aof) = g.aof.as_mut() {
        let synced = aof.end_group().map_err(KevyError::from);
        commit = commit.and(synced);
    }
    commit
}
