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

use std::io;
use std::sync::RwLockWriteGuard;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Inner, Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

/// Handle passed to the `atomic` closure body. Methods mirror the
/// equivalent `Store` ops but operate on the already-held write
/// lock, so reads inside the block see the closure's own writes.
pub struct AtomicCtx<'a> {
    inner: &'a mut Inner,
    log: Vec<Vec<Vec<u8>>>,
}

impl AtomicCtx<'_> {
    // ---- string ops ------------------------------------------------

    /// `SET key value`. Returns `true` (SET always succeeds without
    /// `NX`/`XX` veto).
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> bool {
        let ok = self
            .inner
            .store
            .set(key, value.to_vec(), None, false, false);
        self.log_arg(&[b"SET", key, value]);
        ok
    }

    /// `GET key`.
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.inner
            .store
            .get(key)
            .map(|opt| opt.as_deref().map(<[u8]>::to_vec))
            .map_err(store_err)
    }

    /// `INCR key` — by 1.
    pub fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        let n = self.inner.store.incr_by(key, 1).map_err(store_err)?;
        self.log_arg(&[b"INCR", key]);
        Ok(n)
    }

    /// `INCRBY key delta`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let n = self.inner.store.incr_by(key, delta).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(&[b"INCRBY", key, s.as_bytes()]);
        Ok(n)
    }

    // ---- hash ops ---------------------------------------------------

    /// `HSET key field value`.
    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> io::Result<usize> {
        let owned: Vec<(Vec<u8>, Vec<u8>)> = pairs
            .iter()
            .map(|(f, v)| (f.to_vec(), v.to_vec()))
            .collect();
        let n = self.inner.store.hset(key, &owned).map_err(store_err)?;
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
    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        Ok(self
            .inner
            .store
            .hget(key, field)
            .map_err(store_err)?
            .map(<[u8]>::to_vec))
    }

    /// `HINCRBY key field delta`.
    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> io::Result<i64> {
        let n = self.inner.store.hincrby(key, field, delta).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(&[b"HINCRBY", key, field, s.as_bytes()]);
        Ok(n)
    }

    // ---- zset ops ---------------------------------------------------

    /// `ZADD key score member`.
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> io::Result<usize> {
        let owned: Vec<(f64, Vec<u8>)> =
            pairs.iter().map(|(s, m)| (*s, m.to_vec())).collect();
        let n = self.inner.store.zadd(key, &owned).map_err(store_err)?;
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
    pub fn zincrby(&mut self, key: &[u8], delta: f64, member: &[u8]) -> io::Result<f64> {
        let n = self.inner.store.zincrby(key, delta, member).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(&[b"ZINCRBY", key, s.as_bytes(), member]);
        Ok(n)
    }

    /// `ZSCORE key member`.
    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> io::Result<Option<f64>> {
        self.inner.store.zscore(key, member).map_err(store_err)
    }

    // ---- helpers ----------------------------------------------------

    // ---- keyspace ops (v2.1 — Pipeline write parity) ---------------

    /// `DEL key [key ...]` — every key must hash to this shard.
    pub fn del(&mut self, keys: &[&[u8]]) -> usize {
        let n = self.inner.store.del_borrowed(keys);
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
        keys.iter()
            .filter(|k| self.inner.store.key_exists(k))
            .count()
    }

    // ---- hash ops --------------------------------------------------

    /// `HDEL key field [field ...]`.
    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> io::Result<usize> {
        let owned: Vec<Vec<u8>> = fields.iter().map(|f| f.to_vec()).collect();
        let removed = self.inner.store.hdel(key, &owned).map_err(store_err)?;
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
    pub fn hgetall(&mut self, key: &[u8]) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let flat = self.inner.store.hgetall(key).map_err(store_err)?;
        let mut out = Vec::with_capacity(flat.len() / 2);
        let mut it = flat.into_iter();
        while let (Some(f), Some(v)) = (it.next(), it.next()) {
            out.push((f, v));
        }
        Ok(out)
    }

    /// `HMGET key field [field ...]` — `None` per absent field.
    pub fn hmget(&mut self, key: &[u8], fields: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        self.inner.store.hmget_borrowed(key, fields).map_err(store_err)
    }

    /// `HEXISTS key field`.
    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> io::Result<bool> {
        self.inner.store.hexists(key, field).map_err(store_err)
    }

    // ---- set ops ---------------------------------------------------

    /// `SADD key member [member ...]`.
    pub fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        let owned: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
        let added = self.inner.store.sadd(key, &owned).map_err(store_err)?;
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
    pub fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        let owned: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
        let removed = self.inner.store.srem(key, &owned).map_err(store_err)?;
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
    pub fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        let owned: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
        let len = self.inner.store.lpush(key, &owned).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        argv.push(b"LPUSH");
        argv.push(key);
        argv.extend_from_slice(values);
        self.log_arg(&argv);
        Ok(len)
    }

    /// `RPUSH key value [value ...]` — returns the new list length.
    pub fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        let owned: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
        let len = self.inner.store.rpush(key, &owned).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
        argv.push(b"RPUSH");
        argv.push(key);
        argv.extend_from_slice(values);
        self.log_arg(&argv);
        Ok(len)
    }

    // ---- zset ops --------------------------------------------------

    /// `ZREM key member [member ...]`.
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        let owned: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
        let removed = self.inner.store.zrem(key, &owned).map_err(store_err)?;
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
    pub fn zcard(&mut self, key: &[u8]) -> io::Result<usize> {
        self.inner.store.zcard(key).map_err(store_err)
    }

    /// Flags-aware `ZADD` (v2.1). AOF logs the applied pairs as plain
    /// `ZADD` — the effect, never the condition (deterministic replay).
    pub fn zadd_flags(
        &mut self,
        key: &[u8],
        pairs: &[(f64, &[u8])],
        flags: kevy_store::ZaddFlags,
    ) -> io::Result<kevy_store::ZaddReport> {
        if !flags.valid() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid ZADD flag combo"));
        }
        let rep = self
            .inner
            .store
            .zadd_flags_borrowed(key, pairs, flags)
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
            for (i, (_, m)) in rep.applied.iter().enumerate() {
                parts.push(&score_strs[i]);
                parts.push(m);
            }
            self.log_arg(&parts);
        }
        Ok(rep)
    }

    fn log_arg(&mut self, parts: &[&[u8]]) {
        self.log.push(parts.iter().map(|p| p.to_vec()).collect());
    }
}

impl Store {
    /// Run `body` as a single-shard atomic transaction. Inside the
    /// closure every read sees previous writes; on closure return
    /// the queued AOF writes are committed under one fsync.
    ///
    /// Constraint: every key touched inside the closure must hash to
    /// the same shard. The default embedded config uses 1 shard, so
    /// any key works.
    pub fn atomic<R>(
        &self,
        body: impl FnOnce(&mut AtomicCtx<'_>) -> io::Result<R>,
    ) -> io::Result<R> {
        ensure_writable(self)?;
        let mut g: RwLockWriteGuard<'_, Inner> = self.lock();
        let mut ctx = AtomicCtx { inner: &mut g, log: Vec::new() };
        let r = body(&mut ctx)?;
        // Commit queued AOF writes — one append per op, one fsync at
        // the end via `commit_write`'s standard path.
        let log = std::mem::take(&mut ctx.log);
        for entry in log {
            let parts: Vec<&[u8]> = entry.iter().map(|v| v.as_slice()).collect();
            commit_write(&mut g, &parts)?;
        }
        Ok(r)
    }
}

/// Parity manifest (v2.1): command names `AtomicCtx` implements.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ATOMIC_OPS: &[&str] = &[
    "SET", "GET", "INCR", "INCRBY", "HSET", "HGET", "HINCRBY", "ZADD",
    "ZINCRBY", "ZSCORE", "DEL", "EXISTS", "HDEL", "HGETALL", "HMGET",
    "HEXISTS", "SADD", "SREM", "LPUSH", "RPUSH", "ZREM", "ZCARD",
];
