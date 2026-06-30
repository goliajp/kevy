//! Multi-key string operations, keyspace scan, atomic `getex`, set
//! algebra (`sinter` / `sunion` / `sdiff`), and absolute-time TTL
//! variants (`expireat` / `pexpire`).
//!
//! The set algebra is implemented at the embedded layer (compose
//! `smembers` per key + Rust set operations) instead of touching
//! `kevy_store::Store` — over N small sets that is faster than
//! serialising N RESP arrays.

use std::collections::BTreeSet;
use std::io;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    // ---- multi-key string ops ---------------------------------------

    /// `MSET key value [key value ...]` — set every pair atomically
    /// per-key. Each pair is logged independently to its shard's
    /// AOF (no cross-shard atomic guarantee — a crash mid-call may
    /// leave a prefix applied; matches Redis Cluster semantics).
    pub fn mset(&self, pairs: &[(&[u8], &[u8])]) -> io::Result<()> {
        ensure_writable(self)?;
        for (k, v) in pairs {
            let mut g = self.wshard(k);
            g.store.set(k, v.to_vec(), None, false, false);
            commit_write(&mut g, &[b"SET", k, v])?;
        }
        Ok(())
    }

    /// `MGET key [key ...]` — return `Some(value)` per requested key
    /// that's present, `None` per absent / wrong-type.
    pub fn mget(&self, keys: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push(
                self.wshard(k)
                    .store
                    .get(k)
                    .map_err(store_err)?
                    .as_deref()
                    .map(<[u8]>::to_vec),
            );
        }
        Ok(out)
    }

    // ---- keyspace introspection -------------------------------------

    /// `KEYS pattern` — glob-match every key in the keyspace
    /// (across all shards). `pattern = None` matches everything.
    /// `limit = None` is unbounded; otherwise bounds the TOTAL
    /// returned across shards. Glob syntax matches Redis (`*` /
    /// `?` / `[abc]` / escape).
    pub fn keys(&self, pattern: Option<&[u8]>, limit: Option<usize>) -> Vec<Vec<u8>> {
        self.collect_keys(pattern, limit)
    }

    // ---- atomic get + TTL -------------------------------------------

    /// `GETEX key TTL` — get the value and update the TTL atomically
    /// (single lock cycle on the owning shard). Returns the value;
    /// `None` when absent. AOF-logged as `PEXPIRE`.
    pub fn getex(&self, key: &[u8], ttl: Duration) -> io::Result<Option<Vec<u8>>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let val = g.store.get(key).map_err(store_err)?.as_deref().map(<[u8]>::to_vec);
        if val.is_some() {
            g.store.expire(key, ttl);
            let ttl_ms = ttl.as_millis().min(i64::MAX as u128) as i64;
            let ttl_str = format!("{ttl_ms}");
            commit_write(&mut g, &[b"PEXPIRE", key, ttl_str.as_bytes()])?;
        }
        Ok(val)
    }

    // ---- set algebra (compose-side, not Store-side) ------------------

    /// `SINTER key [key ...]` — set intersection. Reads each key's
    /// members, computes the intersection in BTreeSet order
    /// (sorted, no duplicates).
    pub fn sinter(&self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let first: BTreeSet<Vec<u8>> = self
            .smembers(keys[0])?
            .into_iter()
            .collect();
        let mut acc = first;
        for k in &keys[1..] {
            if acc.is_empty() {
                break;
            }
            let next: BTreeSet<Vec<u8>> = self.smembers(k)?.into_iter().collect();
            acc.retain(|m| next.contains(m));
        }
        Ok(acc.into_iter().collect())
    }

    /// `SUNION key [key ...]` — set union over N sets.
    pub fn sunion(&self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        let mut acc: BTreeSet<Vec<u8>> = BTreeSet::new();
        for k in keys {
            for m in self.smembers(k)? {
                acc.insert(m);
            }
        }
        Ok(acc.into_iter().collect())
    }

    /// `SDIFF key [key ...]` — `keys[0]` minus the union of every
    /// subsequent set.
    pub fn sdiff(&self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut acc: BTreeSet<Vec<u8>> = self
            .smembers(keys[0])?
            .into_iter()
            .collect();
        for k in &keys[1..] {
            let next: BTreeSet<Vec<u8>> = self.smembers(k)?.into_iter().collect();
            acc.retain(|m| !next.contains(m));
        }
        Ok(acc.into_iter().collect())
    }

    // ---- absolute-time TTL variants ----------------------------------

    /// `EXPIREAT key unix_secs` — schedule expiry for the given
    /// absolute UNIX wall-clock time. Returns `true` when the key
    /// existed and the deadline was set; `false` when absent.
    pub fn expireat(&self, key: &[u8], unix_secs: u64) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let unix_ms = unix_secs.saturating_mul(1000);
        let ok = g.store.expire_at_unix_ms(key, unix_ms);
        if ok {
            let ts_str = format!("{unix_ms}");
            commit_write(&mut g, &[b"PEXPIREAT", key, ts_str.as_bytes()])?;
        }
        Ok(ok)
    }

    /// `PEXPIREAT key unix_ms` — same as `expireat` but in
    /// milliseconds.
    pub fn pexpireat(&self, key: &[u8], unix_ms: u64) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.expire_at_unix_ms(key, unix_ms);
        if ok {
            let ts_str = format!("{unix_ms}");
            commit_write(&mut g, &[b"PEXPIREAT", key, ts_str.as_bytes()])?;
        }
        Ok(ok)
    }

    /// `PEXPIRE key ms` — relative TTL in milliseconds. (`expire`
    /// takes `Duration`; this is the integer-ms variant matching
    /// the Redis wire command.)
    pub fn pexpire(&self, key: &[u8], ms: u64) -> io::Result<bool> {
        self.expire(key, Duration::from_millis(ms))
    }

    // ---- hash float increment ----------------------------------------

    /// `HINCRBYFLOAT key field delta` — atomic float increment of a
    /// hash field. Returns the post-increment value. Errors on
    /// `NotFloat` when the field is present but not parseable.
    pub fn hincrbyfloat(
        &self,
        key: &[u8],
        field: &[u8],
        delta: f64,
    ) -> io::Result<f64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_val = g
            .store
            .hincrbyfloat(key, field, delta)
            .map_err(store_err)?;
        let delta_str = format!("{delta}");
        commit_write(&mut g, &[b"HINCRBYFLOAT", key, field, delta_str.as_bytes()])?;
        Ok(new_val)
    }

    // ---- list positional insert --------------------------------------

    /// `LINSERT key BEFORE|AFTER pivot value` — insert `value` before
    /// or after the first occurrence of `pivot` in the list. Returns:
    /// - `Ok(new_len)` on success (`>= 1`);
    /// - `Ok(0)` when `key` does not exist;
    /// - `Ok(-1)` when `pivot` was not found in the list.
    ///
    /// `before = true` matches Redis `LINSERT … BEFORE`, `false`
    /// matches `LINSERT … AFTER`.
    pub fn linsert(
        &self,
        key: &[u8],
        before: bool,
        pivot: &[u8],
        value: &[u8],
    ) -> io::Result<i64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_len = g
            .store
            .linsert(key, before, pivot, value)
            .map_err(store_err)?;
        if new_len > 0 {
            let dir = if before { b"BEFORE".as_slice() } else { b"AFTER".as_slice() };
            commit_write(&mut g, &[b"LINSERT", key, dir, pivot, value])?;
        }
        Ok(new_len)
    }

    // ---- observability ----------------------------------------------

    /// `Store::ping_us()` — return the round-trip duration of a
    /// shard-0 read-lock acquire + release in **nanoseconds**, for
    /// perfgate observability. Always returns immediately; the
    /// duration reflects current shard-0 contention (= shorter when
    /// idle, longer when many readers/writers compete).
    pub fn ping_ns(&self) -> u128 {
        let t = std::time::Instant::now();
        let _g = self.lock();
        t.elapsed().as_nanos()
    }
}
