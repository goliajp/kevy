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

use std::io;
use std::sync::RwLockWriteGuard;

use crate::shard::shard_idx;
use crate::store::{Inner, Store, commit_write, store_err};

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

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
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let i = self.idx(key);
        self.guards[i]
            .store
            .get(key)
            .map(|opt| opt.as_deref().map(<[u8]>::to_vec))
            .map_err(store_err)
    }

    /// `INCR key`.
    pub fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        let i = self.idx(key);
        let n = self.guards[i].store.incr_by(key, 1).map_err(store_err)?;
        self.log_arg(i, &[b"INCR", key]);
        Ok(n)
    }

    /// `INCRBY key delta`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let i = self.idx(key);
        let n = self.guards[i].store.incr_by(key, delta).map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(i, &[b"INCRBY", key, s.as_bytes()]);
        Ok(n)
    }

    // ---- hash ops --------------------------------------------------

    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> io::Result<usize> {
        let i = self.idx(key);
        let owned: Vec<(Vec<u8>, Vec<u8>)> = pairs
            .iter()
            .map(|(f, v)| (f.to_vec(), v.to_vec()))
            .collect();
        let n = self.guards[i]
            .store
            .hset(key, &owned)
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

    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let i = self.idx(key);
        Ok(self.guards[i]
            .store
            .hget(key, field)
            .map_err(store_err)?
            .map(<[u8]>::to_vec))
    }

    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> io::Result<i64> {
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

    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> io::Result<usize> {
        let i = self.idx(key);
        let owned: Vec<(f64, Vec<u8>)> =
            pairs.iter().map(|(s, m)| (*s, m.to_vec())).collect();
        let n = self.guards[i]
            .store
            .zadd(key, &owned)
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

    pub fn zincrby(&mut self, key: &[u8], delta: f64, member: &[u8]) -> io::Result<f64> {
        let i = self.idx(key);
        let n = self.guards[i]
            .store
            .zincrby(key, delta, member)
            .map_err(store_err)?;
        let s = format!("{delta}");
        self.log_arg(i, &[b"ZINCRBY", key, s.as_bytes(), member]);
        Ok(n)
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
        body: impl FnOnce(&mut AtomicAllShards<'_>) -> io::Result<R>,
    ) -> io::Result<R> {
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
