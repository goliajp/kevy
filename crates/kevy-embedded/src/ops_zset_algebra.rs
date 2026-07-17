//! zset algebra facades (Redis 6.2): `ZINTERSTORE` / `ZUNIONSTORE` /
//! `ZDIFFSTORE` / `ZINTERCARD`, plus the set-algebra `*STORE` forms
//! (`SINTERSTORE` / `SUNIONSTORE` / `SDIFFSTORE`) that 1.6.0 shipped
//! only as reads.
//!
//! Locking: sources are read under their own shard locks sequentially,
//! then `dst` is written under its shard lock — same non-atomic
//! window as [`Store::copy`] (a concurrent writer between the reads
//! and the store can be observed). For a fully atomic combination use
//! the same ops inside [`Store::atomic_all_shards`].
//!
//! AOF: the **effect** is logged (`DEL dst` + plain `ZADD`/`SADD` of
//! the result), never the combination — deterministic on replay and
//! replica-apply regardless of source state.

use crate::KevyResult;

use kevy_store::{ZAggregate, zdiff, zinter, zintercard, zunion};

/// One source key's scored members (sets contribute score 1.0).
type ScoredInput = Vec<(Vec<u8>, f64)>;

use crate::store::ensure_writable;
use crate::store::{Store, commit_write, store_err};

impl Store {
    fn gather_scored(&self, keys: &[&[u8]]) -> KevyResult<Vec<ScoredInput>> {
        keys.iter()
            .map(|k| self.wshard(k).store.zset_or_set_members(k).map_err(store_err))
            .collect()
    }

    fn store_zset_result(&self, dst: &[u8], pairs: &[(Vec<u8>, f64)]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(dst);
        let n = g.store.zstore_result(dst, pairs);
        commit_write(&mut g, &[b"DEL", dst])?;
        if !pairs.is_empty() {
            let score_strs: Vec<Vec<u8>> =
                pairs.iter().map(|(_, s)| format!("{s}").into_bytes()).collect();
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
            argv.push(b"ZADD");
            argv.push(dst);
            for (i, (m, _)) in pairs.iter().enumerate() {
                argv.push(&score_strs[i]);
                argv.push(m);
            }
            commit_write(&mut g, &argv)?;
        }
        Ok(n)
    }

    /// `ZINTERSTORE dst keys… [WEIGHTS …] [AGGREGATE SUM|MIN|MAX]` —
    /// returns the stored cardinality.
    pub fn zinterstore(
        &self,
        dst: &[u8],
        keys: &[&[u8]],
        weights: Option<&[f64]>,
        aggregate: ZAggregate,
    ) -> KevyResult<usize> {
        let inputs = self.gather_scored(keys)?;
        self.store_zset_result(dst, &zinter(&inputs, weights, aggregate))
    }

    /// `ZUNIONSTORE dst keys… [WEIGHTS …] [AGGREGATE …]`.
    pub fn zunionstore(
        &self,
        dst: &[u8],
        keys: &[&[u8]],
        weights: Option<&[f64]>,
        aggregate: ZAggregate,
    ) -> KevyResult<usize> {
        let inputs = self.gather_scored(keys)?;
        self.store_zset_result(dst, &zunion(&inputs, weights, aggregate))
    }

    /// `ZDIFFSTORE dst keys…` (no weights/aggregate — Redis 6.2).
    pub fn zdiffstore(&self, dst: &[u8], keys: &[&[u8]]) -> KevyResult<usize> {
        let inputs = self.gather_scored(keys)?;
        self.store_zset_result(dst, &zdiff(&inputs))
    }

    /// `ZINTERCARD keys… [LIMIT n]` — `limit = 0` means unlimited.
    pub fn zintercard(&self, keys: &[&[u8]], limit: usize) -> KevyResult<usize> {
        let inputs = self.gather_scored(keys)?;
        Ok(zintercard(&inputs, limit))
    }

    /// `SINTERSTORE dst keys…` — set-algebra store form (members only).
    pub fn sinterstore(&self, dst: &[u8], keys: &[&[u8]]) -> KevyResult<usize> {
        let members = self.sinter(keys)?;
        self.store_set_result(dst, &members)
    }

    /// `SUNIONSTORE dst keys…`.
    pub fn sunionstore(&self, dst: &[u8], keys: &[&[u8]]) -> KevyResult<usize> {
        let members = self.sunion(keys)?;
        self.store_set_result(dst, &members)
    }

    /// `SDIFFSTORE dst keys…`.
    pub fn sdiffstore(&self, dst: &[u8], keys: &[&[u8]]) -> KevyResult<usize> {
        let members = self.sdiff(keys)?;
        self.store_set_result(dst, &members)
    }

    fn store_set_result(&self, dst: &[u8], members: &[Vec<u8>]) -> KevyResult<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(dst);
        g.store.del(&[dst]);
        commit_write(&mut g, &[b"DEL", dst])?;
        if members.is_empty() {
            return Ok(0);
        }
        let member_refs: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
        let n = g.store.sadd(dst, &member_refs).map_err(store_err)?;
        let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + members.len());
        argv.push(b"SADD");
        argv.push(dst);
        argv.extend(members.iter().map(Vec::as_slice));
        commit_write(&mut g, &argv)?;
        Ok(n)
    }
}
