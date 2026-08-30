//! Collection reads for [`AtomicAllShards`]. Split from
//! `ops_atomic_all.rs` for the 500-LOC house rule; the single-shard
//! twins live on `AtomicCtx` in `ops_atomic.rs`.
//!
//! A consumer reshaped an entire keyspace from sets to hashes because a
//! set could be written inside a transaction but never read back inside
//! one. These hold the shard write lock already, so there was never a
//! consistency reason to withhold them.

use super::AtomicAllShards;
use crate::KevyResult;
use crate::store_glue::store_err;

impl AtomicAllShards<'_> {
    // ---- collection reads --------------------------------------------
    // Mirror of `AtomicCtx`'s — see the note there. The parity manifests
    // below are checked by a test because these two ctxs drifted before.

    /// `SMEMBERS key`.
    pub fn smembers(&mut self, key: &[u8]) -> KevyResult<Vec<Vec<u8>>> {
        let i = self.idx(key);
        self.guards[i].store.smembers(key).map_err(store_err)
    }

    /// `SISMEMBER key member`.
    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> KevyResult<bool> {
        let i = self.idx(key);
        self.guards[i].store.sismember(key, member).map_err(store_err)
    }

    /// `LRANGE key start stop`.
    pub fn lrange(&mut self, key: &[u8], start: i64, stop: i64) -> KevyResult<Vec<Vec<u8>>> {
        let i = self.idx(key);
        self.guards[i].store.lrange(key, start, stop).map_err(store_err)
    }

    /// `LLEN key`.
    pub fn llen(&mut self, key: &[u8]) -> KevyResult<usize> {
        let i = self.idx(key);
        self.guards[i].store.llen(key).map_err(store_err)
    }

    /// `SCARD key`.
    pub fn scard(&mut self, key: &[u8]) -> KevyResult<usize> {
        let i = self.idx(key);
        self.guards[i].store.scard(key).map_err(store_err)
    }

    /// `ZRANGEBYSCORE key min max`.
    pub fn zrangebyscore(
        &mut self,
        key: &[u8],
        min: kevy_store::ScoreBound,
        max: kevy_store::ScoreBound,
    ) -> KevyResult<Vec<(Vec<u8>, f64)>> {
        let i = self.idx(key);
        self.guards[i].store.zrange_by_score(key, min, max).map_err(store_err)
    }
}
