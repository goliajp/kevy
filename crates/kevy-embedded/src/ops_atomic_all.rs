//! Cross-shard read-modify-write closure:
//! `Store::atomic_all_shards`.
//!
//! `atomic_all_shards(|tx| { ... })` holds a write lock on every
//! shard for the closure body. Operations inside the closure are
//! routed to their owning shards, and AOF writes are batched
//! per-shard with one fsync per shard at commit time.
//!
//! Heavier than [`Store::atomic`](crate::Store::atomic): every
//! reader and writer on the affected shards blocks until the
//! closure returns. Use it only when the closure genuinely needs
//! more than one shard and atomicity across them is required.

use crate::{KevyError, KevyResult};
use std::sync::RwLockWriteGuard;

use crate::shard::shard_idx;
use crate::store::{Inner, Store, commit_write, store_err};

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> KevyResult<()> { Ok(()) }

/// Context handed to the `atomic_all_shards` closure body. Methods
/// route to the right shard by hashing the key.
pub struct AtomicAllShards<'a> {
    guards: Vec<RwLockWriteGuard<'a, Inner>>,
    /// (shard_idx, serialised RESP-frame parts) queued for AOF commit.
    log: Vec<(usize, Vec<Vec<u8>>)>,
}

impl<'a> AtomicAllShards<'a> {
    fn idx(&self, key: &[u8]) -> usize {
        shard_idx(key, self.guards.len())
    }

    fn log_arg(&mut self, idx: usize, parts: &[&[u8]]) {
        self.log
            .push((idx, parts.iter().map(|p| p.to_vec()).collect()));
    }

    // ---- string ops -----------------------------------------------

    /// `SET key value` — always succeeds.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> bool {
        let i = self.idx(key);
        let ok = self.guards[i]
            .store
            .set(key, value.to_vec(), None, false, false);
        self.log_arg(i, &[b"SET", key, value]);
        ok
    }

    /// `GET key`.
    pub fn get(&mut self, key: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        let i = self.idx(key);
        self.guards[i]
            .store
            .get(key)
            .map(|opt| opt.as_deref().map(<[u8]>::to_vec))
            .map_err(store_err)
    }

    /// `INCR key`.
    pub fn incr(&mut self, key: &[u8]) -> KevyResult<i64> {
        let i = self.idx(key);
        let n = self.guards[i].store.incr_by(key, 1).map_err(store_err)?;
        self.log_arg(i, &[b"INCR", key]);
        Ok(n)
    }

    /// `INCRBY key delta`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> KevyResult<i64> {
        let i = self.idx(key);
        let n = self.guards[i].store.incr_by(key, delta).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(i, &[b"INCRBY", key, s.as_bytes()]);
        Ok(n)
    }

    // ---- hash ops --------------------------------------------------

    /// `HSET key field value [field value ...]`. Returns count newly
    /// added (existing fields are overwritten but not counted).
    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> KevyResult<usize> {
        let i = self.idx(key);
        let n = self.guards[i]
            .store
            .hset(key, pairs)
            .map_err(store_err)?;
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        parts.push(b"HSET");
        parts.push(key);
        for (f, v) in pairs {
            parts.push(f);
            parts.push(v);
        }
        self.log_arg(i, &parts);
        Ok(n)
    }

    /// `HGET key field` — `None` when the key or field is absent.
    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        let i = self.idx(key);
        Ok(self.guards[i]
            .store
            .hget(key, field)
            .map_err(store_err)?
            .map(<[u8]>::to_vec))
    }

    /// `HINCRBY key field delta` — returns the field's new value.
    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> KevyResult<i64> {
        let i = self.idx(key);
        let n = self.guards[i]
            .store
            .hincrby(key, field, delta)
            .map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(i, &[b"HINCRBY", key, field, s.as_bytes()]);
        Ok(n)
    }

    // ---- zset ops --------------------------------------------------

    /// `ZADD key score member [score member ...]`. Returns count newly
    /// added (score updates of existing members are not counted).
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> KevyResult<usize> {
        let i = self.idx(key);
        let n = self.guards[i]
            .store
            .zadd(key, pairs)
            .map_err(store_err)?;
        let score_strs: Vec<Vec<u8>> = pairs
            .iter()
            .map(|(s, _)| format!("{s}").into_bytes())
            .collect();
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        parts.push(b"ZADD");
        parts.push(key);
        for (j, (_, m)) in pairs.iter().enumerate() {
            parts.push(&score_strs[j]);
            parts.push(m);
        }
        self.log_arg(i, &parts);
        Ok(n)
    }

    /// `ZINCRBY key delta member` — returns the member's new score.
    pub fn zincrby(&mut self, key: &[u8], delta: f64, member: &[u8]) -> KevyResult<f64> {
        let i = self.idx(key);
        let n = self.guards[i]
            .store
            .zincrby(key, delta, member)
            .map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(i, &[b"ZINCRBY", key, s.as_bytes(), member]);
        Ok(n)
    }

    /// `ZSCORE key member` (v2.1: parity with [`super::ops_atomic::AtomicCtx`]).
    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> KevyResult<Option<f64>> {
        let i = self.idx(key);
        self.guards[i].store.zscore(key, member).map_err(store_err)
    }

    // ---- keyspace ops (v2.1 — Pipeline write parity) ---------------

    /// `DEL key [key ...]` — keys may span shards; each key's delete
    /// is applied and AOF-logged on its own shard.
    pub fn del(&mut self, keys: &[&[u8]]) -> usize {
        let mut n = 0;
        for k in keys {
            let i = self.idx(k);
            if self.guards[i].store.del(&[k]) > 0 {
                n += 1;
                self.log_arg(i, &[b"DEL", k]);
            }
        }
        n
    }

    /// `EXISTS key [key ...]` — count of the given keys that exist.
    pub fn exists(&mut self, keys: &[&[u8]]) -> usize {
        keys.iter()
            .filter(|k| {
                let i = self.idx(k);
                self.guards[i].store.key_exists(k)
            })
            .count()
    }

    // ---- hash ops --------------------------------------------------

    /// `HDEL key field [field ...]`.
    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<usize> {
        let i = self.idx(key);
        let removed = self.guards[i].store.hdel(key, fields).map_err(store_err)?;
        if removed > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + fields.len());
            argv.push(b"HDEL");
            argv.push(key);
            argv.extend_from_slice(fields);
            self.log_arg(i, &argv);
        }
        Ok(removed)
    }

    /// `HGETALL key` — `(field, value)` pairs.
    pub fn hgetall(&mut self, key: &[u8]) -> KevyResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let i = self.idx(key);
        let flat = self.guards[i].store.hgetall(key).map_err(store_err)?;
        let mut out = Vec::with_capacity(flat.len() / 2);
        let mut it = flat.into_iter();
        while let (Some(f), Some(v)) = (it.next(), it.next()) {
            out.push((f, v));
        }
        Ok(out)
    }

    /// `HMGET key field [field ...]` — `None` per absent field.
    pub fn hmget(&mut self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<Option<Vec<u8>>>> {
        let i = self.idx(key);
        self.guards[i].store.hmget(key, fields).map_err(store_err)
    }

    /// `HEXISTS key field`.
    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> KevyResult<bool> {
        let i = self.idx(key);
        self.guards[i].store.hexists(key, field).map_err(store_err)
    }

    // ---- set ops ---------------------------------------------------

    /// `SADD key member [member ...]`.
    pub fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        let i = self.idx(key);
        let added = self.guards[i].store.sadd(key, members).map_err(store_err)?;
        if added > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            argv.push(b"SADD");
            argv.push(key);
            argv.extend_from_slice(members);
            self.log_arg(i, &argv);
        }
        Ok(added)
    }

    /// `SREM key member [member ...]`.
    pub fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        let i = self.idx(key);
        let removed = self.guards[i].store.srem(key, members).map_err(store_err)?;
        if removed > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            argv.push(b"SREM");
            argv.push(key);
            argv.extend_from_slice(members);
            self.log_arg(i, &argv);
        }
        Ok(removed)
    }

    // ---- list ops --------------------------------------------------

    /// `LPUSH key value [value ...]` — returns the new list length.
    pub fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> KevyResult<usize> {
        let i = self.idx(key);
        let len = self.guards[i].store.lpush(key, values).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        argv.push(b"LPUSH");
        argv.push(key);
        argv.extend_from_slice(values);
        self.log_arg(i, &argv);
        Ok(len)
    }

    /// `RPUSH key value [value ...]` — returns the new list length.
    pub fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> KevyResult<usize> {
        let i = self.idx(key);
        let len = self.guards[i].store.rpush(key, values).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        argv.push(b"RPUSH");
        argv.push(key);
        argv.extend_from_slice(values);
        self.log_arg(i, &argv);
        Ok(len)
    }

    // ---- zset ops --------------------------------------------------

    /// `ZREM key member [member ...]`.
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        let i = self.idx(key);
        let removed = self.guards[i].store.zrem(key, members).map_err(store_err)?;
        if removed > 0 {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            argv.push(b"ZREM");
            argv.push(key);
            argv.extend_from_slice(members);
            self.log_arg(i, &argv);
        }
        Ok(removed)
    }

    /// `ZCARD key` — member count; 0 when absent.
    pub fn zcard(&mut self, key: &[u8]) -> KevyResult<usize> {
        let i = self.idx(key);
        self.guards[i].store.zcard(key).map_err(store_err)
    }

    /// Flags-aware `ZADD` (v2.1). AOF logs the applied pairs as plain
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
        let i = self.idx(key);
        let rep = self.guards[i]
            .store
            .zadd_flags(key, pairs, flags)
            .map_err(store_err)?;
        if !rep.applied.is_empty() {
            let score_strs: Vec<Vec<u8>> = rep
                .applied
                .iter()
                .map(|(s, _)| format!("{s}").into_bytes())
                .collect();
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + rep.applied.len() * 2);
            parts.push(b"ZADD");
            parts.push(key);
            for (j, (_, m)) in rep.applied.iter().enumerate() {
                parts.push(&score_strs[j]);
                parts.push(m);
            }
            self.log_arg(i, &parts);
        }
        Ok(rep)
    }
}

impl Store {
    /// Run `body` as a transaction holding write locks on EVERY
    /// shard for the closure's duration. Reads inside the closure
    /// see prior writes (full read-modify-write). On closure
    /// return, AOF writes commit with one fsync per shard.
    ///
    /// Cost: blocks every other writer + reader on this Store for
    /// the closure body. Use when atomic multi-shard semantics are
    /// required; otherwise prefer the single-shard `atomic`.
    pub fn atomic_all_shards<R>(
        &self,
        body: impl FnOnce(&mut AtomicAllShards<'_>) -> KevyResult<R>,
    ) -> KevyResult<R> {
        ensure_writable(self)?;
        // Take every shard's write lock in shard-index order
        // (deterministic order avoids deadlock).
        let guards: Vec<RwLockWriteGuard<'_, Inner>> = self
            .shards
            .iter()
            .map(|s| s.write().expect("lock poisoned"))
            .collect();
        let mut ctx = AtomicAllShards { guards, log: Vec::new() };
        let r = body(&mut ctx)?;
        // Commit AOF entries per-shard.
        let log = std::mem::take(&mut ctx.log);
        for (idx, parts) in log {
            let g = &mut ctx.guards[idx];
            let refs: Vec<&[u8]> = parts.iter().map(|v| v.as_slice()).collect();
            commit_write(g, &refs)?;
        }
        Ok(r)
    }
}

/// Parity manifest (v2.1): command names `AtomicAllShards` implements.
/// MUST stay identical to `ops_atomic::ATOMIC_OPS` (the two ctxs
/// drifted before — zscore was missing here).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ATOMIC_ALL_OPS: &[&str] = &[
    "SET", "GET", "INCR", "INCRBY", "HSET", "HGET", "HINCRBY", "ZADD",
    "ZINCRBY", "ZSCORE", "DEL", "EXISTS", "HDEL", "HGETALL", "HMGET",
    "HEXISTS", "SADD", "SREM", "LPUSH", "RPUSH", "ZREM", "ZCARD",
];
