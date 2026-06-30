//! Atomic single-shard transaction closure (kevy-embedded 1.10.0).
//!
//! `atomic(|tx| { ... })` runs the closure body holding a write
//! lock on shard 0 for its entire duration. Inside the closure
//! every read sees previous writes, so read-modify-write loops
//! work as expected. All AOF writes are deferred + batched into a
//! single fsync at commit time.
//!
//! Single-shard scope: every key touched inside the closure must
//! hash to shard 0. The default embedded config (1 shard) matches.
//! Multi-shard atomic transactions would block every writer for
//! the closure's duration — defer to a future ship if the use case
//! demands it.
//!
//! Lives outside `ops.rs` to keep the file under the 500-LOC
//! house rule.

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
