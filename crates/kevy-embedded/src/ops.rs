//! Data-type methods on [`Store`] — string, hash, list, set, sorted set,
//! plus the pub/sub `publish` / `subscribe` / `psubscribe` entry points.
//!
//! All of these are thin facades over `kevy_store::Store` (the keyspace)
//! and `pubsub::PubsubBus` (the in-process bus); they hold the embedded
//! mutex for the duration of the underlying call, then drop it. AOF
//! logging + post-write eviction sweep run via `commit_write` from
//! `store.rs`. Behaviour and ABI are unchanged from the v1.1.0 single-file
//! layout — this module only exists to keep `store.rs` under the 500-LOC
//! cap.

use std::io;
use std::time::Duration;

use kevy_store::StoreError;

use crate::pubsub::Subscription;
#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

// wasm has no replica runner, so the read-only guard is unconditionally a
// no-op. Mirrors `replica_glue::ensure_writable`'s signature so the rest of
// this file is target-agnostic.
#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    // ---- string ops -----------------------------------------------------

    /// `SET key value` (no TTL, no NX/XX). Returns `true` always under the
    /// embedded API (Redis semantics: SET overwrites; NX/XX vetoes would
    /// return `false` but we don't expose those here — use [`Store::with`]
    /// for the full surface).
    pub fn set(&self, key: &[u8], value: &[u8]) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.set(key, value.to_vec(), None, false, false);
        commit_write(&mut g, &[b"SET", key, value])?;
        Ok(ok)
    }

    /// `SET key value PX ms` — overwrites + sets TTL. The AOF records an
    /// **absolute** `PEXPIREAT` deadline (not the relative `ttl`) so the key
    /// expires at the same wall-clock instant after a restart — a relative
    /// `PEXPIRE` would be re-anchored to replay-time, resetting the TTL to a
    /// fresh full duration on every restart (INC-2026-06-09).
    pub fn set_with_ttl(&self, key: &[u8], value: &[u8], ttl: Duration) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.set(key, value.to_vec(), Some(ttl), false, false);
        let ms = ttl.as_millis().min(u128::from(u64::MAX)) as u64;
        let deadline = kevy_store::now_unix_ms().saturating_add(ms);
        commit_write(&mut g, &[b"SET", key, value])?;
        commit_write(&mut g, &[b"PEXPIREAT", key, deadline.to_string().as_bytes()])?;
        Ok(ok)
    }

    /// `GET key` — `Some(bytes)` on hit, `None` on miss or expired.
    ///
    /// With eviction off (`maxmemory == 0`, the default) this takes the **read**
    /// lock and a non-mutating store lookup, so concurrent readers scale across
    /// cores — the path a read-heavy embed cache lives on. With eviction on it
    /// falls back to the exclusive lock + mutating get so each access still
    /// stamps the LRU clock.
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if self.config().maxmemory == 0 {
            let g = self.rshard(key);
            return Ok(g.store.get_shared(key).map_err(store_err)?.map(|c| c.into_owned()));
        }
        let mut g = self.wshard(key);
        Ok(g.store.get(key).map_err(store_err)?.map(|c| c.into_owned()))
    }

    /// `DEL key1 [key2 ...]`. Returns the count of keys actually removed.
    /// Keys fan out to their owning shards.
    pub fn del(&self, keys: &[&[u8]]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut total = 0;
        for k in keys {
            let owned = vec![k.to_vec()];
            let mut g = self.wshard(k);
            let n = g.store.del(&owned);
            if n > 0 {
                total += n;
                commit_write(&mut g, &[b"DEL", k])?;
            }
        }
        Ok(total)
    }

    /// `EXISTS key1 [key2 ...]`. Count of existing keys (duplicates counted
    /// multiple times, matching Redis).
    pub fn exists(&self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut total = 0;
        for k in keys {
            total += self.wshard(k).store.exists(&[k.to_vec()]);
        }
        Ok(total)
    }

    /// `INCR key`. Returns the post-increment value.
    pub fn incr(&self, key: &[u8]) -> io::Result<i64> {
        self.incr_by(key, 1)
    }

    /// `INCRBY key delta`. Negative `delta` does DECR-style work.
    pub fn incr_by(&self, key: &[u8], delta: i64) -> io::Result<i64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let n = g.store.incr_by(key, delta).map_err(store_err)?;
        commit_write(&mut g, &[b"INCRBY", key, delta.to_string().as_bytes()])?;
        Ok(n)
    }

    /// `EXPIRE key seconds`. Returns `true` if a key was touched. The AOF
    /// records an absolute `PEXPIREAT` deadline (see [`Self::set_with_ttl`])
    /// so the TTL survives a restart unchanged.
    pub fn expire(&self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let touched = g.store.expire(key, ttl);
        if touched {
            let ms = ttl.as_millis().min(u128::from(u64::MAX)) as u64;
            let deadline = kevy_store::now_unix_ms().saturating_add(ms);
            commit_write(&mut g, &[b"PEXPIREAT", key, deadline.to_string().as_bytes()])?;
        }
        Ok(touched)
    }

    /// `PERSIST key`. Returns `true` if a TTL was actually cleared.
    pub fn persist(&self, key: &[u8]) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let touched = g.store.persist(key);
        if touched {
            commit_write(&mut g, &[b"PERSIST", key])?;
        }
        Ok(touched)
    }

    /// Remaining TTL in ms (or Redis-style `-1`/`-2` for no-TTL/no-key).
    pub fn ttl_ms(&self, key: &[u8]) -> i64 {
        self.wshard(key).store.pttl(key)
    }

    /// `TYPE key` — `"string"`, `"hash"`, `"list"`, `"set"`, `"zset"`, or `"none"`.
    pub fn type_of(&self, key: &[u8]) -> &'static str {
        self.wshard(key).store.type_of(key)
    }

    /// `DBSIZE` — total live keys across all shards.
    pub fn dbsize(&self) -> usize {
        self.sum_shards(|i| i.store.dbsize())
    }

    /// `FLUSHALL` — empty every shard (each logs `FLUSHALL` so a replay reaches
    /// the same empty state).
    ///
    /// Named `flushall` — **not** `flush` — to avoid colliding with
    /// `Write::flush`'s "sync buffered writes to disk" meaning. This call
    /// WIPES the store; durability needs no explicit call (each write appends
    /// to the AOF, the shard's `BufWriter` lands per [`AppendFsync`] cadence
    /// and on drop).
    ///
    /// [`AppendFsync`]: crate::AppendFsync
    pub fn flushall(&self) -> io::Result<()> {
        ensure_writable(self)?;
        self.try_for_each_shard(|inner| {
            inner.store.flushall();
            commit_write(inner, &[b"FLUSHALL"])
        })
    }

    /// Deprecated alias for [`Self::flushall`]. The old name read like
    /// `Write::flush` (sync-to-disk) but actually WIPES the store — a
    /// data-loss footgun.
    #[deprecated(
        since = "1.2.0",
        note = "renamed to `flushall`: `flush` collides with Write::flush (sync-to-disk); this WIPES the store"
    )]
    pub fn flush(&self) -> io::Result<()> {
        self.flushall()
    }

    /// `MEMORY USAGE` for one key — `Some(bytes)` or `None` if absent.
    pub fn key_bytes(&self, key: &[u8]) -> Option<u64> {
        self.wshard(key).store.estimate_key_bytes(key)
    }

    /// Live `used_memory` estimate (summed across shards).
    pub fn used_memory(&self) -> u64 {
        self.sum_shards_u64(|i| i.store.used_memory())
    }

    /// `INFO`-style counter: total keys evicted by `maxmemory` (all shards).
    pub fn evictions_total(&self) -> u64 {
        self.sum_shards_u64(|i| i.store.evictions_total())
    }

    /// `INFO`-style counter: total keys expired (lazy + active reaper, all shards).
    pub fn expired_keys_total(&self) -> u64 {
        self.sum_shards_u64(|i| i.store.expired_keys_total())
    }

    // ---- hash ops -------------------------------------------------------

    /// `HSET key field value [field value ...]`. Returns count newly added.
    pub fn hset(&self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let owned: Vec<(Vec<u8>, Vec<u8>)> =
            pairs.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect();
        let added = g.store.hset(key, &owned).map_err(store_err)?;
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        parts.push(b"HSET");
        parts.push(key);
        for (f, v) in pairs {
            parts.push(f);
            parts.push(v);
        }
        commit_write(&mut g, &parts)?;
        Ok(added)
    }

    /// `HGET key field`. `None` if absent.
    pub fn hget(&self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let mut g = self.wshard(key);
        Ok(g.store
            .hget(key, field)
            .map_err(store_err)?
            .map(<[u8]>::to_vec))
    }

    /// `HDEL key field [field ...]`. Returns count actually removed.
    pub fn hdel(&self, key: &[u8], fields: &[&[u8]]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let owned: Vec<Vec<u8>> = fields.iter().map(|f| f.to_vec()).collect();
        let removed = g.store.hdel(key, &owned).map_err(store_err)?;
        if removed > 0 {
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + fields.len());
            parts.push(b"HDEL");
            parts.push(key);
            for f in fields {
                parts.push(f);
            }
            commit_write(&mut g, &parts)?;
        }
        Ok(removed)
    }

    // ---- list ops -------------------------------------------------------

    /// `LPUSH key value [value ...]`. Returns the new list length.
    pub fn lpush(&self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        push_helper(self, key, values, b"LPUSH", kevy_store::Store::lpush)
    }

    /// `RPUSH key value [value ...]`. Returns the new list length.
    pub fn rpush(&self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        push_helper(self, key, values, b"RPUSH", kevy_store::Store::rpush)
    }

    /// `LPOP key count`. Returns popped values from the head.
    pub fn lpop(&self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        pop_helper(self, key, count, false)
    }

    /// `RPOP key count`. Symmetric to `LPOP` from the tail.
    pub fn rpop(&self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        pop_helper(self, key, count, true)
    }

    /// `LLEN key`. Length of the list at `key`; 0 if absent.
    pub fn llen(&self, key: &[u8]) -> io::Result<usize> {
        self.wshard(key).store.llen(key).map_err(store_err)
    }

    // ---- set ops --------------------------------------------------------

    /// `SADD key member [member ...]`. Returns count newly added.
    pub fn sadd(&self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        push_helper(self, key, members, b"SADD", kevy_store::Store::sadd)
    }

    /// `SREM key member [member ...]`. Returns count actually removed.
    pub fn srem(&self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let owned: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
        let removed = g.store.srem(key, &owned).map_err(store_err)?;
        if removed > 0 {
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            parts.push(b"SREM");
            parts.push(key);
            for m in members {
                parts.push(m);
            }
            commit_write(&mut g, &parts)?;
        }
        Ok(removed)
    }

    /// `SMEMBERS key`. Order implementation-defined; empty if absent.
    pub fn smembers(&self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.wshard(key).store.smembers(key).map_err(store_err)
    }

    /// `SCARD key`. Member count; 0 if absent.
    pub fn scard(&self, key: &[u8]) -> io::Result<usize> {
        self.wshard(key).store.scard(key).map_err(store_err)
    }

    // ---- zset ops -------------------------------------------------------

    /// `ZADD key score member [score member ...]`. Returns count newly added.
    pub fn zadd(&self, key: &[u8], pairs: &[(f64, &[u8])]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let owned: Vec<(f64, Vec<u8>)> =
            pairs.iter().map(|(s, m)| (*s, m.to_vec())).collect();
        let added = g.store.zadd(key, &owned).map_err(store_err)?;
        let mut score_strs: Vec<Vec<u8>> = Vec::with_capacity(pairs.len());
        for (s, _) in pairs {
            score_strs.push(format!("{s}").into_bytes());
        }
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        parts.push(b"ZADD");
        parts.push(key);
        for (i, (_, m)) in pairs.iter().enumerate() {
            parts.push(&score_strs[i]);
            parts.push(m);
        }
        commit_write(&mut g, &parts)?;
        Ok(added)
    }

    /// `ZREM key member [member ...]`. Returns count actually removed.
    pub fn zrem(&self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let owned: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
        let removed = g.store.zrem(key, &owned).map_err(store_err)?;
        if removed > 0 {
            let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
            parts.push(b"ZREM");
            parts.push(key);
            for m in members {
                parts.push(m);
            }
            commit_write(&mut g, &parts)?;
        }
        Ok(removed)
    }

    /// `ZSCORE key member`. `Some(score)` if present.
    pub fn zscore(&self, key: &[u8], member: &[u8]) -> io::Result<Option<f64>> {
        self.wshard(key).store.zscore(key, member).map_err(store_err)
    }

    /// `ZCARD key`. Member count; 0 if absent.
    pub fn zcard(&self, key: &[u8]) -> io::Result<usize> {
        self.wshard(key).store.zcard(key).map_err(store_err)
    }

    // ---- pub/sub --------------------------------------------------------

    /// `PUBLISH channel payload`. Delivers `payload` to every subscriber on
    /// `channel` (direct + pattern matches) inside this process. Returns
    /// the count of receivers the message reached.
    pub fn publish(&self, channel: &[u8], payload: &[u8]) -> usize {
        // Clone matching senders under the lock, then release before
        // send() so a slow receiver can't stall unrelated traffic.
        let plans = {
            // Pub/sub is process-wide; the bus lives on shard 0.
            let g = self.lock();
            g.bus.collect_delivery(channel, payload)
        };
        let mut count = 0;
        for (frame, sender) in plans {
            if sender.send(frame).is_ok() {
                count += 1;
            }
        }
        count
    }

    /// Open a [`Subscription`] subscribed to `channels`. Drop the handle
    /// to unsubscribe from everything atomically. Pass `&[]` to start
    /// with no subscriptions and add some later via
    /// [`Subscription::subscribe`] / [`Subscription::psubscribe`].
    pub fn subscribe(&self, channels: &[&[u8]]) -> Subscription {
        let mut sub = Subscription::new(self.inner_handle(), self.guard_handle());
        if !channels.is_empty() {
            sub.subscribe(channels);
        }
        sub
    }

    /// Convenience: open a [`Subscription`] starting on pattern subscriptions.
    pub fn psubscribe(&self, patterns: &[&[u8]]) -> Subscription {
        let mut sub = Subscription::new(self.inner_handle(), self.guard_handle());
        if !patterns.is_empty() {
            sub.psubscribe(patterns);
        }
        sub
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Shared list/set push + list pop helpers. `&Store` so we can lock + AOF-log.
// ─────────────────────────────────────────────────────────────────────────

fn push_helper<F>(
    s: &Store,
    key: &[u8],
    values: &[&[u8]],
    verb: &'static [u8],
    op: F,
) -> io::Result<usize>
where
    F: FnOnce(&mut kevy_store::Store, &[u8], &[Vec<u8>]) -> Result<usize, StoreError>,
{
    ensure_writable(s)?;
    let mut g = s.wshard(key);
    let owned: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();
    let n = op(&mut g.store, key, &owned).map_err(store_err)?;
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
    parts.push(verb);
    parts.push(key);
    for v in values {
        parts.push(v);
    }
    commit_write(&mut g, &parts)?;
    Ok(n)
}

fn pop_helper(s: &Store, key: &[u8], count: usize, from_tail: bool) -> io::Result<Vec<Vec<u8>>> {
    ensure_writable(s)?;
    let mut g = s.wshard(key);
    let popped = if from_tail {
        g.store.rpop(key, count).map_err(store_err)?
    } else {
        g.store.lpop(key, count).map_err(store_err)?
    };
    if !popped.is_empty() {
        let verb: &[u8] = if from_tail { b"RPOP" } else { b"LPOP" };
        let count_str = popped.len().to_string();
        let parts: [&[u8]; 3] = [verb, key, count_str.as_bytes()];
        commit_write(&mut g, &parts)?;
    }
    Ok(popped)
}
