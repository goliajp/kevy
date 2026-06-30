//! Phase 2 ops — mailrs feedback round-out (2026-07-01).
//!
//! Adds the `hgetall` / `hmget` / `hkeys` / `hvals` / `hexists` /
//! `hlen` / `hincrby`, `zrange` / `zrevrange` / `zincrby`,
//! `lrange` / `lindex` / `lrem`, and `getset` / `getdel` methods to
//! the embedded `Store` facade.
//!
//! Lives outside `ops.rs` to keep that file under the 500-LOC house
//! rule. Every method is a thin facade over the existing
//! `kevy_store::Store::<method>` (the keyspace) plus the standard
//! `commit_write` AOF logging on the write paths.

use std::io;

use kevy_store::ScoreBound;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    // ---- hash mass-getters --------------------------------------------

    /// `HGETALL key` — every `(field, value)` pair in `key`'s hash, in
    /// arbitrary order. Empty when `key` is absent. Errors on wrong type.
    pub fn hgetall(&self, key: &[u8]) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let flat = self.wshard(key).store.hgetall(key).map_err(store_err)?;
        // kevy-store returns [f0, v0, f1, v1, ...] — pair them up.
        let mut out = Vec::with_capacity(flat.len() / 2);
        let mut it = flat.into_iter();
        while let (Some(f), Some(v)) = (it.next(), it.next()) {
            out.push((f, v));
        }
        Ok(out)
    }

    /// `HEXISTS key field` — `true` when `field` is present.
    pub fn hexists(&self, key: &[u8], field: &[u8]) -> io::Result<bool> {
        self.wshard(key).store.hexists(key, field).map_err(store_err)
    }

    /// `HLEN key` — number of fields; 0 when absent.
    pub fn hlen(&self, key: &[u8]) -> io::Result<usize> {
        self.wshard(key).store.hlen(key).map_err(store_err)
    }

    /// `HKEYS key` — every field name in `key`'s hash.
    pub fn hkeys(&self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.wshard(key).store.hkeys(key).map_err(store_err)
    }

    /// `HVALS key` — every value in `key`'s hash.
    pub fn hvals(&self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.wshard(key).store.hvals(key).map_err(store_err)
    }

    /// `HMGET key field [field ...]` — read multiple fields in one
    /// call. `None` per requested field that is absent.
    pub fn hmget(&self, key: &[u8], fields: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        let owned: Vec<Vec<u8>> = fields.iter().map(|f| f.to_vec()).collect();
        self.wshard(key).store.hmget(key, &owned).map_err(store_err)
    }

    /// `HINCRBY key field delta` — atomic integer increment of a hash
    /// field. Returns the post-increment value.
    pub fn hincrby(&self, key: &[u8], field: &[u8], delta: i64) -> io::Result<i64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_val = g.store.hincrby(key, field, delta).map_err(store_err)?;
        let delta_str = format!("{delta}");
        commit_write(&mut g, &[b"HINCRBY", key, field, delta_str.as_bytes()])?;
        Ok(new_val)
    }

    // ---- zset mass-readers + atomic incr -----------------------------

    /// `ZRANGE key start stop WITHSCORES` — members in ascending score
    /// order between rank `start..=stop` (Redis-style inclusive
    /// indexing; negatives count from the tail). Returns `(member,
    /// score)` pairs.
    pub fn zrange(&self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<(Vec<u8>, f64)>> {
        self.wshard(key).store.zrange(key, start, stop).map_err(store_err)
    }

    /// `ZREVRANGE key start stop WITHSCORES` — `zrange` with the order
    /// reversed (highest score first). The `start..=stop` indexing is
    /// against the reversed list, matching Redis semantics.
    pub fn zrevrange(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> io::Result<Vec<(Vec<u8>, f64)>> {
        let mut all = self
            .wshard(key)
            .store
            .zrange(key, 0, -1)
            .map_err(store_err)?;
        all.reverse();
        let n = all.len() as i64;
        if n == 0 {
            return Ok(Vec::new());
        }
        let clamp = |x: i64| -> usize {
            let v = if x < 0 { (n + x).max(0) } else { x.min(n - 1) };
            v as usize
        };
        let s = clamp(start);
        let e = clamp(stop);
        if s > e {
            return Ok(Vec::new());
        }
        Ok(all.into_iter().skip(s).take(e - s + 1).collect())
    }

    /// `ZRANGEBYSCORE` — score-range read. `min` / `max` are
    /// inclusive; pass `f64::NEG_INFINITY` / `f64::INFINITY` for open
    /// bounds. Returns `(member, score)` pairs in ascending score
    /// order. Exclusive bounds are `ZRANGEBYSCORE (` syntax in Redis;
    /// expose via the dedicated [`Self::zrange_by_score_excl`].
    pub fn zrange_by_score(
        &self,
        key: &[u8],
        min: f64,
        max: f64,
    ) -> io::Result<Vec<(Vec<u8>, f64)>> {
        self.wshard(key)
            .store
            .zrange_by_score(
                key,
                ScoreBound { value: min, exclusive: false },
                ScoreBound { value: max, exclusive: false },
            )
            .map_err(store_err)
    }

    /// Same as [`Self::zrange_by_score`] but with explicit
    /// inclusive/exclusive control on each bound (`(min` / `(max` in
    /// Redis syntax).
    pub fn zrange_by_score_excl(
        &self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
    ) -> io::Result<Vec<(Vec<u8>, f64)>> {
        self.wshard(key)
            .store
            .zrange_by_score(key, min, max)
            .map_err(store_err)
    }

    /// `ZINCRBY key delta member` — atomic float increment of a member's
    /// score. Returns the post-increment score.
    pub fn zincrby(&self, key: &[u8], delta: f64, member: &[u8]) -> io::Result<f64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_score = g.store.zincrby(key, delta, member).map_err(store_err)?;
        let delta_str = format!("{delta}");
        commit_write(&mut g, &[b"ZINCRBY", key, delta_str.as_bytes(), member])?;
        Ok(new_score)
    }

    // ---- list slice + index ops --------------------------------------

    /// `LRANGE key start stop` — list slice. Negative indices count
    /// from the tail. Empty when absent.
    pub fn lrange(&self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<Vec<u8>>> {
        self.wshard(key).store.lrange(key, start, stop).map_err(store_err)
    }

    /// `LINDEX key idx` — element at index `idx`; `None` out of range.
    pub fn lindex(&self, key: &[u8], idx: i64) -> io::Result<Option<Vec<u8>>> {
        self.wshard(key).store.lindex(key, idx).map_err(store_err)
    }

    /// `LREM key count value` — remove up to `|count|` occurrences of
    /// `value`. `count > 0` from head, `count < 0` from tail,
    /// `count == 0` all. Returns the count actually removed.
    pub fn lrem(&self, key: &[u8], count: i64, value: &[u8]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let removed = g.store.lrem(key, count, value).map_err(store_err)?;
        if removed > 0 {
            let count_str = format!("{count}");
            commit_write(&mut g, &[b"LREM", key, count_str.as_bytes(), value])?;
        }
        Ok(removed)
    }

    // ---- string single-call atomic patterns --------------------------

    /// `GETSET key new` — set `key` to `new`, return the previous
    /// value (or `None` when `key` was absent).
    pub fn getset(&self, key: &[u8], new: &[u8]) -> io::Result<Option<Vec<u8>>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let prev = g.store.getset(key, new.to_vec()).map_err(store_err)?;
        commit_write(&mut g, &[b"SET", key, new])?;
        Ok(prev)
    }

    /// `GETDEL key` — delete `key`, return the previous value
    /// (`None` when `key` was absent).
    pub fn getdel(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let prev = g.store.getdel(key).map_err(store_err)?;
        if prev.is_some() {
            commit_write(&mut g, &[b"DEL", key])?;
        }
        Ok(prev)
    }
}
