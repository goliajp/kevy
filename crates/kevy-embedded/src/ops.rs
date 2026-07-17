//! Data-type methods on [`Store`] — string, hash, list, set, sorted set,
//! plus the pub/sub `publish` / `subscribe` / `psubscribe` entry points.
//!
//! All of these are thin facades over `kevy_store::Store` (the keyspace)
//! and `pubsub::PubsubBus` (the in-process bus); they hold the embedded
//! mutex for the duration of the underlying call, then drop it. AOF
//! logging + post-write eviction sweep run via `commit_write` from
//! `store.rs`. Behaviour and ABI are unchanged from the original
//! single-file layout — this module only exists to keep `store.rs` under
//! the 500-LOC cap.

use crate::KevyResult;
use std::time::Duration;

use kevy_store::StoreError;

use crate::pubsub::Subscription;
use crate::store::ensure_writable;
use crate::store::{Store, commit_write, store_err};

// wasm has no replica runner, so the read-only guard is unconditionally a
// no-op. Mirrors `replica_glue::ensure_writable`'s signature so the rest of
// this file is target-agnostic.
impl Store {
    // ---- string ops -----------------------------------------------------

    /// `SET key value` (no TTL, no NX/XX). Returns `true` always under the
    /// embedded API (Redis semantics: SET overwrites; NX/XX vetoes would
    /// return `false` but we don't expose those here — use [`Store::with`]
    /// for the full surface).
    pub fn set(&self, key: &[u8], value: &[u8]) -> KevyResult<bool> {
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
    /// fresh full duration on every restart (seen as a production
    /// incident: cache keys never expired across restarts).
    pub fn set_with_ttl(&self, key: &[u8], value: &[u8], ttl: Duration) -> KevyResult<bool> {
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
    /// The lock is **policy-gated** (see [`Self::reads_use_shared_lock`]):
    /// whenever the active eviction policy won't consume a per-read LRU/LFU
    /// tick — `maxmemory == 0` (the default), or the `NoEviction` /
    /// `*Random` / `VolatileTtl` policies — this takes the **shared** lock and a
    /// non-mutating [`get_shared`](kevy_store::Store::get_shared) lookup. That
    /// is a lock-*correctness* choice: a read-only GET has no business holding
    /// the exclusive lock and blocking a concurrent writer on its shard. It is
    /// **not** a throughput win — concurrent GETs still contend on the shard's
    /// `RwLock` word (there is no lock-free read path), so read scaling is
    /// bounded by shard count, not core count. Expired keys still read as
    /// `None` here (lazy expire-skip; the reaper / next write reclaims them).
    /// Only the true LRU/LFU policies fall back to the exclusive lock + mutating
    /// get so each access stamps the clock the eviction scorer ranks by.
    pub fn get(&self, key: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        if self.reads_use_shared_lock() {
            let g = self.rshard(key);
            return Ok(g.store.get_shared(key).map_err(store_err)?.map(|c| c.into_owned()));
        }
        let mut g = self.wshard(key);
        Ok(g.store.get(key).map_err(store_err)?.map(|c| c.into_owned()))
    }

    /// Whether the GET read-lane can take the SHARED shard lock rather than the
    /// exclusive one — true when no per-read LRU/LFU tick would be consumed:
    /// `maxmemory == 0` (eviction never runs), or the `NoEviction` / `*Random`
    /// / `VolatileTtl` policies (whose scorer ignores the per-read clock —
    /// writes still tick the clock the `*Random` policies sample). Only the
    /// `*Lru` / `*Lfu` policies need the exclusive lock to stamp the access clock.
    fn reads_use_shared_lock(&self) -> bool {
        let p = self.config().eviction_policy;
        self.config().maxmemory == 0 || !(p.uses_lru() || p.uses_lfu())
    }

    /// `GET` for the FFI zero-copy *shared* lane (`kevy_get_shared`). Bulk
    /// values come back as an `Arc::clone` — **no byte copy** — so the FFI can
    /// hand JS a buffer viewing the engine's own storage (the win vs the plain
    /// [`Self::get`], which `into_owned`-copies). The underlying lookup is
    /// non-mutating, so the lock is policy-gated exactly like [`Self::get`]
    /// (see [`Self::reads_use_shared_lock`]): the SHARED shard lock whenever no
    /// per-read LRU/LFU tick would be consumed, else the exclusive lock (this
    /// lane never stamps the LRU clock regardless).
    pub fn get_shared_owned(&self, key: &[u8]) -> KevyResult<Option<kevy_store::GetShared>> {
        if self.reads_use_shared_lock() {
            let g = self.rshard(key);
            return g.store.get_shared_owned(key).map_err(store_err);
        }
        let g = self.wshard(key);
        g.store.get_shared_owned(key).map_err(store_err)
    }

    /// `DEL key1 [key2 ...]`. Returns the count of keys actually removed.
    /// Keys fan out to their owning shards.
    pub fn del(&self, keys: &[&[u8]]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut total = 0;
        for k in keys {
            let mut g = self.wshard(k);
            let n = g.store.del(&[*k]);
            if n > 0 {
                total += n;
                commit_write(&mut g, &[b"DEL", k])?;
            }
        }
        Ok(total)
    }

    /// `EXISTS key1 [key2 ...]`. Count of existing keys (duplicates counted
    /// multiple times, matching Redis).
    pub fn exists(&self, keys: &[&[u8]]) -> KevyResult<usize> {
        let mut total = 0;
        for k in keys {
            total += self.wshard(k).store.exists(&[*k]);
        }
        Ok(total)
    }

    /// `INCR key`. Returns the post-increment value.
    pub fn incr(&self, key: &[u8]) -> KevyResult<i64> {
        self.incr_by(key, 1)
    }

    /// `INCRBY key delta`. Negative `delta` does DECR-style work.
    pub fn incr_by(&self, key: &[u8], delta: i64) -> KevyResult<i64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let n = g.store.incr_by(key, delta).map_err(store_err)?;
        commit_write(&mut g, &[b"INCRBY", key, delta.to_string().as_bytes()])?;
        Ok(n)
    }

    /// `EXPIRE key seconds`. Returns `true` if a key was touched. The AOF
    /// records an absolute `PEXPIREAT` deadline (see [`Self::set_with_ttl`])
    /// so the TTL survives a restart unchanged.
    pub fn expire(&self, key: &[u8], ttl: Duration) -> KevyResult<bool> {
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
    pub fn persist(&self, key: &[u8]) -> KevyResult<bool> {
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
    ///
    /// Aggregates under each shard's SHARED lock (the underlying `dbsize` is
    /// `&self`). This is a latency fix, not a scaling one: a full-keyspace
    /// count shouldn't hold every shard's *write* lock and stall concurrent
    /// writers while it sums.
    pub fn dbsize(&self) -> usize {
        self.sum_shards_read(|i| i.store.dbsize())
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
    pub fn flushall(&self) -> KevyResult<()> {
        ensure_writable(self)?;
        let r = self.try_for_each_shard(|inner| {
            inner.store.flushall();
            commit_write(inner, &[b"FLUSHALL"])
        });
        // CDC feed contract: FLUSHALL breaks stream continuity.
        #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
        self.feed_bump_on_flush();
        r
    }

    /// `MEMORY USAGE` for one key — `Some(bytes)` or `None` if absent.
    ///
    /// Read-only (`estimate_key_bytes` is `&self`), so it takes the shard's
    /// SHARED lock rather than blocking a concurrent writer on that shard.
    pub fn key_bytes(&self, key: &[u8]) -> Option<u64> {
        self.rshard(key).store.estimate_key_bytes(key)
    }

    /// Live `used_memory` estimate (summed across shards).
    ///
    /// Aggregates under each shard's SHARED lock (latency fix — an INFO-time
    /// memory sum shouldn't hold every shard's write lock; see [`Self::dbsize`]).
    pub fn used_memory(&self) -> u64 {
        self.sum_shards_u64_read(|i| i.store.used_memory())
    }

    /// `INFO`-style counter: total keys evicted by `maxmemory` (all shards).
    ///
    /// Read-only aggregation under each shard's SHARED lock (see [`Self::dbsize`]).
    pub fn evictions_total(&self) -> u64 {
        self.sum_shards_u64_read(|i| i.store.evictions_total())
    }

    /// `INFO`-style counter: total keys expired (lazy + active reaper, all shards).
    ///
    /// Read-only aggregation under each shard's SHARED lock (see [`Self::dbsize`]).
    pub fn expired_keys_total(&self) -> u64 {
        self.sum_shards_u64_read(|i| i.store.expired_keys_total())
    }

    // ---- hash ops -------------------------------------------------------

    /// `HSET key field value [field value ...]`. Returns count newly added.
    pub fn hset(&self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let added = g.store.hset(key, pairs).map_err(store_err)?;
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
    pub fn hget(&self, key: &[u8], field: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        let mut g = self.wshard(key);
        Ok(g.store
            .hget(key, field)
            .map_err(store_err)?
            .map(<[u8]>::to_vec))
    }

    /// `HDEL key field [field ...]`. Returns count actually removed.
    pub fn hdel(&self, key: &[u8], fields: &[&[u8]]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let removed = g.store.hdel(key, fields).map_err(store_err)?;
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
    pub fn lpush(&self, key: &[u8], values: &[&[u8]]) -> KevyResult<usize> {
        push_helper(self, key, values, b"LPUSH", kevy_store::Store::lpush)
    }

    /// `RPUSH key value [value ...]`. Returns the new list length.
    pub fn rpush(&self, key: &[u8], values: &[&[u8]]) -> KevyResult<usize> {
        push_helper(self, key, values, b"RPUSH", kevy_store::Store::rpush)
    }

    /// `LPOP key count`. Returns popped values from the head.
    pub fn lpop(&self, key: &[u8], count: usize) -> KevyResult<Vec<Vec<u8>>> {
        pop_helper(self, key, count, false)
    }

    /// `RPOP key count`. Symmetric to `LPOP` from the tail.
    pub fn rpop(&self, key: &[u8], count: usize) -> KevyResult<Vec<Vec<u8>>> {
        pop_helper(self, key, count, true)
    }

    /// `LLEN key`. Length of the list at `key`; 0 if absent.
    pub fn llen(&self, key: &[u8]) -> KevyResult<usize> {
        self.wshard(key).store.llen(key).map_err(store_err)
    }

    // ---- set ops --------------------------------------------------------

    /// `SADD key member [member ...]`. Returns count newly added.
    pub fn sadd(&self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        push_helper(self, key, members, b"SADD", kevy_store::Store::sadd)
    }

    /// `SREM key member [member ...]`. Returns count actually removed.
    pub fn srem(&self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let removed = g.store.srem(key, members).map_err(store_err)?;
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
    pub fn smembers(&self, key: &[u8]) -> KevyResult<Vec<Vec<u8>>> {
        self.wshard(key).store.smembers(key).map_err(store_err)
    }

    /// `SCARD key`. Member count; 0 if absent.
    pub fn scard(&self, key: &[u8]) -> KevyResult<usize> {
        self.wshard(key).store.scard(key).map_err(store_err)
    }

    // ---- zset ops -------------------------------------------------------

    /// `ZADD key score member [score member ...]`. Returns count newly added.
    pub fn zadd(&self, key: &[u8], pairs: &[(f64, &[u8])]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let added = g.store.zadd(key, pairs).map_err(store_err)?;
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
    pub fn zrem(&self, key: &[u8], members: &[&[u8]]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let removed = g.store.zrem(key, members).map_err(store_err)?;
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
    pub fn zscore(&self, key: &[u8], member: &[u8]) -> KevyResult<Option<f64>> {
        self.wshard(key).store.zscore(key, member).map_err(store_err)
    }

    /// `ZCARD key`. Member count; 0 if absent.
    pub fn zcard(&self, key: &[u8]) -> KevyResult<usize> {
        self.wshard(key).store.zcard(key).map_err(store_err)
    }

    // ---- pub/sub --------------------------------------------------------

    /// Dispatch one command as argv, appending the RESP-encoded reply to
    /// `out` — the full read+write verb surface (`ESTORE_OPS` plus the
    /// conn face). This is the generic entry the FFI layer builds every
    /// language binding on: one function reaches the whole engine. The
    /// read-only listener keeps its own narrower whitelist.
    pub fn dispatch_argv(&self, argv: &[Vec<u8>], out: &mut Vec<u8>) {
        crate::dispatch::dispatch(self, argv, out);
    }

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
) -> KevyResult<usize>
where
    F: FnOnce(&mut kevy_store::Store, &[u8], &[&[u8]]) -> Result<usize, StoreError>,
{
    ensure_writable(s)?;
    let mut g = s.wshard(key);
    let n = op(&mut g.store, key, values).map_err(store_err)?;
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + values.len());
    parts.push(verb);
    parts.push(key);
    for v in values {
        parts.push(v);
    }
    commit_write(&mut g, &parts)?;
    Ok(n)
}

fn pop_helper(s: &Store, key: &[u8], count: usize, from_tail: bool) -> KevyResult<Vec<Vec<u8>>> {
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
