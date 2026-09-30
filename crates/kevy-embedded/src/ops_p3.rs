//! Multi-key string operations, keyspace scan, atomic `getex`, set
//! algebra (`sinter` / `sunion` / `sdiff`), and absolute-time TTL
//! variants (`expireat` / `pexpire`).
//!
//! The set algebra is implemented at the embedded layer (compose
//! `smembers` per key + Rust set operations) instead of touching
//! `kevy_store::Store` — over N small sets that is faster than
//! serialising N RESP arrays.

use crate::{InsertPosition, KevyResult};
use std::collections::BTreeSet;
use std::time::Duration;

use crate::store::ensure_writable;
use crate::store::{Store, commit_write, store_err};

impl Store {
    // ---- multi-key string ops ---------------------------------------

    /// `MSET key value [key value ...]` — set every pair. The pairs of one
    /// shard are set under one lock and logged as one `MSET` frame, so a
    /// crash leaves each shard's share whole or absent; across shards there
    /// is no such guarantee (Redis Cluster's semantics). A key given twice
    /// takes its last value.
    pub fn mset(&self, pairs: &[(&[u8], &[u8])]) -> KevyResult<()> {
        ensure_writable(self)?;
        let n = self.shards.len();
        if n == 1 {
            return self.mset_shard(0, pairs);
        }
        let mut by_shard: Vec<Vec<(&[u8], &[u8])>> = vec![Vec::new(); n];
        for &(k, v) in pairs {
            by_shard[crate::shard::shard_idx(k, n)].push((k, v));
        }
        for (i, group) in by_shard.iter().enumerate().filter(|(_, g)| !g.is_empty()) {
            self.mset_shard(i, group)?;
        }
        Ok(())
    }

    fn mset_shard(&self, i: usize, pairs: &[(&[u8], &[u8])]) -> KevyResult<()> {
        let mut g = crate::store::lock_write(&self.shards[i]);
        let mut frame: Vec<&[u8]> = Vec::with_capacity(1 + 2 * pairs.len());
        frame.push(b"MSET");
        for &(k, v) in pairs {
            g.store.set(k, v.to_vec(), None, kevy_store::SetCondition::Always);
            frame.extend([k, v]);
        }
        commit_write(&mut g, &frame)
    }

    /// `MGET key [key ...]` — return `Some(value)` per requested key
    /// that's present, `None` per absent / wrong-type.
    ///
    /// The wrong-type half of that sentence was prose only: the read
    /// propagated the store's `WrongType` with `?`, so one list among
    /// the keys turned the whole call into an error. Redis returns nil
    /// for a key that does not hold a string and never errors here —
    /// which is what the server's gather already did, and how the two
    /// surfaces came to disagree in `differential_wire_vs_embedded`.
    pub fn mget(&self, keys: &[&[u8]]) -> KevyResult<Vec<Option<Vec<u8>>>> {
        let mut out = Vec::with_capacity(keys.len());
        self.mget_with(keys.iter().copied(), |v| out.push(v.map(<[u8]>::to_vec)))?;
        Ok(out)
    }

    /// [`Store::mget`] that lends each value to `f`, in key order, instead
    /// of copying it out — for a caller that packs the values somewhere of
    /// its own (a binding's reply buffer) and would otherwise copy twice.
    ///
    /// ```
    /// let s = kevy_embedded::Store::open(kevy_embedded::Config::default()).unwrap();
    /// s.set(b"a", b"1").unwrap();
    /// let mut seen = Vec::new();
    /// s.mget_with([&b"a"[..], b"nope"], |v| seen.push(v.map(<[u8]>::len))).unwrap();
    /// assert_eq!(seen, [Some(1), None]);
    /// ```
    pub fn mget_with<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k [u8]>,
        mut f: impl FnMut(Option<&[u8]>),
    ) -> KevyResult<()> {
        for k in keys {
            match self.wshard(k).store.get(k) {
                Ok(v) => f(v.as_deref()),
                Err(kevy_store::StoreError::WrongType) => f(None),
                // Not reachable through a hot keyspace, and it stays:
                // `Store::get` propagates the tiering path's errors
                // too, and a cold-tier read that failed must not be
                // answered as "this key holds nothing".
                Err(e) => return Err(store_err(e)),
            }
        }
        Ok(())
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
    /// `None` when absent. AOF-logged as an absolute `PEXPIREAT` — a
    /// relative frame re-anchors on every replay, handing the key its
    /// full TTL back at each restart (the incident class the absolute
    /// form exists to prevent; this was its last relative holdout).
    pub fn getex(&self, key: &[u8], ttl: Duration) -> KevyResult<Option<Vec<u8>>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let val = g.store.get(key).map_err(store_err)?.as_deref().map(<[u8]>::to_vec);
        if val.is_some() {
            g.store.expire(key, ttl);
            crate::store_glue::commit_deadline(&mut g, key)?;
        }
        Ok(val)
    }

    // ---- set algebra (compose-side, not Store-side) ------------------

    /// `SINTER key [key ...]` — set intersection. Reads each key's
    /// members, computes the intersection in BTreeSet order
    /// (sorted, no duplicates).
    pub fn sinter(&self, keys: &[&[u8]]) -> KevyResult<Vec<Vec<u8>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let first: BTreeSet<Vec<u8>> = self.smembers(keys[0])?.into_iter().collect();
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
    pub fn sunion(&self, keys: &[&[u8]]) -> KevyResult<Vec<Vec<u8>>> {
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
    pub fn sdiff(&self, keys: &[&[u8]]) -> KevyResult<Vec<Vec<u8>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut acc: BTreeSet<Vec<u8>> = self.smembers(keys[0])?.into_iter().collect();
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
    pub fn expireat(&self, key: &[u8], unix_secs: u64) -> KevyResult<bool> {
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
    pub fn pexpireat(&self, key: &[u8], unix_ms: u64) -> KevyResult<bool> {
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
    pub fn pexpire(&self, key: &[u8], ms: u64) -> KevyResult<bool> {
        self.expire(key, Duration::from_millis(ms))
    }

    // ---- hash float increment ----------------------------------------

    /// `HINCRBYFLOAT key field delta` — atomic float increment of a
    /// hash field. Returns the post-increment value. Errors on
    /// `NotFloat` when the field is present but not parseable.
    pub fn hincrbyfloat(&self, key: &[u8], field: &[u8], delta: f64) -> KevyResult<f64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_val = g.store.hincrbyfloat(key, field, delta).map_err(store_err)?;
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
    /// ```
    /// use kevy_embedded::{Config, InsertPosition, Store};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.rpush(b"l", &[b"a", b"c"])?;
    /// assert_eq!(s.linsert(b"l", InsertPosition::Before, b"c", b"b")?, 3);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub fn linsert(
        &self,
        key: &[u8],
        position: InsertPosition,
        pivot: &[u8],
        value: &[u8],
    ) -> KevyResult<i64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_len = g.store.linsert(key, position, pivot, value).map_err(store_err)?;
        if new_len > 0 {
            let dir = match position {
                InsertPosition::Before => b"BEFORE".as_slice(),
                InsertPosition::After => b"AFTER".as_slice(),
            };
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
