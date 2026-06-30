//! More Redis-standard ops — sismember / spop / srandmember,
//! zrank / zcount / zpopmin / zrem_range_by_{rank,score} /
//! zrev_range_by_score, lset / ltrim, rename / renamenx
//! (kevy-embedded 1.11.0).
//!
//! Net-additive round-out. Every method here wraps an existing
//! `kevy_store::Store` method that already exists at the keyspace
//! level.
//!
//! Lives outside `ops.rs` / `ops_p2.rs` / `ops_p3.rs` / `ops_bitmap.rs` /
//! `ops_bonus.rs` / `ops_scan.rs` / `ops_atomic.rs` / `ops_pipeline.rs`
//! to keep all files under the 500-LOC house rule.

use std::io;

use kevy_store::ScoreBound;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    // ---- set extras --------------------------------------------------

    /// `SISMEMBER key member` — `true` when `member` is in the set.
    pub fn sismember(&self, key: &[u8], member: &[u8]) -> io::Result<bool> {
        self.wshard(key).store.sismember(key, member).map_err(store_err)
    }

    /// `SPOP key count` — atomically remove + return up to `count`
    /// random members.
    pub fn spop(&self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let popped = g.store.spop(key, count).map_err(store_err)?;
        if !popped.is_empty() {
            let count_str = format!("{count}");
            commit_write(&mut g, &[b"SPOP", key, count_str.as_bytes()])?;
        }
        Ok(popped)
    }

    /// `SRANDMEMBER key count` — return up to `count` random members
    /// without removing them.
    pub fn srandmember(&self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        self.wshard(key).store.srandmember(key, count).map_err(store_err)
    }

    // ---- sorted set extras ------------------------------------------

    /// `ZRANK key member` — rank (0-based, ascending) of `member`;
    /// `None` if not present.
    pub fn zrank(&self, key: &[u8], member: &[u8]) -> io::Result<Option<usize>> {
        self.wshard(key).store.zrank(key, member).map_err(store_err)
    }

    /// `ZCOUNT key min max` — count members whose score falls in
    /// `[min, max]` (inclusive). Pass `±INFINITY` for open bounds.
    pub fn zcount(&self, key: &[u8], min: f64, max: f64) -> io::Result<usize> {
        self.wshard(key)
            .store
            .zcount(
                key,
                ScoreBound { value: min, exclusive: false },
                ScoreBound { value: max, exclusive: false },
            )
            .map_err(store_err)
    }

    /// `ZPOPMIN key count` — atomically remove + return up to `count`
    /// members with the lowest scores. Pairs are `(member, score)`.
    pub fn zpopmin(&self, key: &[u8], count: usize) -> io::Result<Vec<(Vec<u8>, f64)>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let popped = g.store.zpopmin(key, count).map_err(store_err)?;
        if !popped.is_empty() {
            let count_str = format!("{count}");
            commit_write(&mut g, &[b"ZPOPMIN", key, count_str.as_bytes()])?;
        }
        Ok(popped)
    }

    /// `ZREMRANGEBYRANK key start stop` — remove members in
    /// `[start, stop]` rank range (inclusive, Redis-style negative
    /// indexing). Returns count removed.
    pub fn zremrangebyrank(
        &self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let removed = g.store.zrem_range_by_rank(key, start, stop).map_err(store_err)?;
        if removed > 0 {
            let s = format!("{start}");
            let e = format!("{stop}");
            commit_write(&mut g, &[b"ZREMRANGEBYRANK", key, s.as_bytes(), e.as_bytes()])?;
        }
        Ok(removed)
    }

    /// `ZREMRANGEBYSCORE key min max` — remove members with scores
    /// in `[min, max]` (inclusive). Returns count removed.
    pub fn zremrangebyscore(
        &self,
        key: &[u8],
        min: f64,
        max: f64,
    ) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let removed = g
            .store
            .zrem_range_by_score(
                key,
                ScoreBound { value: min, exclusive: false },
                ScoreBound { value: max, exclusive: false },
            )
            .map_err(store_err)?;
        if removed > 0 {
            let s = format!("{min}");
            let e = format!("{max}");
            commit_write(&mut g, &[b"ZREMRANGEBYSCORE", key, s.as_bytes(), e.as_bytes()])?;
        }
        Ok(removed)
    }

    /// `ZREVRANGEBYSCORE key max min` — members with scores in
    /// `[min, max]` in DESCENDING score order. Inclusive bounds.
    pub fn zrev_range_by_score(
        &self,
        key: &[u8],
        max: f64,
        min: f64,
    ) -> io::Result<Vec<(Vec<u8>, f64)>> {
        self.wshard(key)
            .store
            .zrev_range_by_score(
                key,
                ScoreBound { value: min, exclusive: false },
                ScoreBound { value: max, exclusive: false },
            )
            .map_err(store_err)
    }

    // ---- list extras -------------------------------------------------

    /// `LSET key idx value` — set the element at `idx` (negative
    /// indexes count from tail). Errors `NoSuchKey` / `OutOfRange`
    /// matching Redis.
    pub fn lset(&self, key: &[u8], idx: i64, value: &[u8]) -> io::Result<()> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        g.store.lset(key, idx, value).map_err(store_err)?;
        let i = format!("{idx}");
        commit_write(&mut g, &[b"LSET", key, i.as_bytes(), value])?;
        Ok(())
    }

    /// `LTRIM key start stop` — trim list to `[start, stop]`
    /// inclusive (Redis-style negative indexing).
    pub fn ltrim(&self, key: &[u8], start: i64, stop: i64) -> io::Result<()> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        g.store.ltrim(key, start, stop).map_err(store_err)?;
        let s = format!("{start}");
        let e = format!("{stop}");
        commit_write(&mut g, &[b"LTRIM", key, s.as_bytes(), e.as_bytes()])?;
        Ok(())
    }

    // ---- keyspace extras --------------------------------------------

    /// `RENAME src dst` — atomic rename. Returns `true` when the
    /// rename happened. Errors when `src` doesn't exist (Redis would
    /// reply `-ERR no such key`, here `Err(NoSuchKey)`).
    pub fn rename(&self, src: &[u8], dst: &[u8]) -> io::Result<bool> {
        ensure_writable(self)?;
        // Cross-shard rename is non-trivial; the single-shard
        // embedded default lands src+dst on the same lock.
        let mut g = self.wshard(src);
        let outcome = g.store.rename(src, dst, false);
        match outcome {
            kevy_store::RenameOutcome::Renamed => {
                commit_write(&mut g, &[b"RENAME", src, dst])?;
                Ok(true)
            }
            kevy_store::RenameOutcome::NoSuchSrc => {
                Err(io::Error::other("no such key"))
            }
            kevy_store::RenameOutcome::DstExists => Ok(false),
        }
    }

    /// `RENAMENX src dst` — rename only when `dst` doesn't exist.
    /// Returns `true` when the rename happened.
    pub fn renamenx(&self, src: &[u8], dst: &[u8]) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(src);
        let outcome = g.store.rename(src, dst, true);
        match outcome {
            kevy_store::RenameOutcome::Renamed => {
                commit_write(&mut g, &[b"RENAMENX", src, dst])?;
                Ok(true)
            }
            kevy_store::RenameOutcome::DstExists => Ok(false),
            kevy_store::RenameOutcome::NoSuchSrc => {
                Err(io::Error::other("no such key"))
            }
        }
    }
}
