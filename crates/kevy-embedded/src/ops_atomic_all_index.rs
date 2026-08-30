//! Index reads for [`AtomicAllShards`].
//!
//! A consumer asked to check uniqueness inside a transaction against a
//! declared index instead of a parallel set of claim keys. Claim keys
//! are a second source of truth that has to be reconciled at boot; an
//! index is derived by construction and cannot drift.
//!
//! **Only the all-shards context gets these, deliberately.** An index
//! entry lives on the shard of the key it indexes, so "does any row
//! have this email" is a question about every shard. `atomic()` holds
//! one shard's lock and can answer only for its own slice — and a
//! uniqueness check that silently consults 1/N of the keyspace is worse
//! than no uniqueness check, because it returns "unique" most of the
//! time. `atomic_all_shards()` already holds every shard's write lock,
//! so the same read is both complete and consistent there.
//!
//! **These see committed state, not the transaction's own writes.**
//! Index maintenance runs in `commit_write`, which runs after the
//! closure returns `Ok`. Checking a value against rows written earlier
//! in the same closure will not find them; a closure inserting two rows
//! must compare them to each other itself. The lock makes this the only
//! gap — nothing another writer does can appear mid-transaction.

use super::AtomicAllShards;
use crate::KevyResult;
use crate::ops_index::{IndexPage, merge_page};
use crate::ops_index_sync::sync_segs;
use kevy_index::{Cursor, IndexValue};

impl AtomicAllShards<'_> {
    /// `IDX.QUERY name min max` — range or EQ, merged across every
    /// shard in `(value, key)` order.
    ///
    /// Sees the index as of the last commit; see the module note.
    pub fn idx_query(
        &mut self,
        name: &[u8],
        min: &IndexValue,
        max: &IndexValue,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> KevyResult<IndexPage> {
        let limit = limit.clamp(1, 100_000);
        let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
        self.each_segment(name, |seg| {
            let (hits, _) = seg.range(min, max, cursor, limit);
            all.extend(hits.into_iter().map(|(k, v)| (v, k)));
        })?;
        Ok(merge_page(all, limit))
    }

    /// `IDX.COUNT name min max` without materialising keys — the cheap
    /// form of an existence check.
    pub fn idx_count(
        &mut self,
        name: &[u8],
        min: &IndexValue,
        max: &IndexValue,
    ) -> KevyResult<u64> {
        let mut total = 0u64;
        self.each_segment(name, |seg| total += seg.count(min, max))?;
        Ok(total)
    }

    /// Visit `name`'s segment on every shard, using the write guards
    /// this transaction already holds. The `Store` twin takes each
    /// shard lock itself, which is why it cannot be reused here — this
    /// context holds them all, and re-locking would deadlock.
    fn each_segment(
        &mut self,
        name: &[u8],
        mut f: impl FnMut(&kevy_index::Segment),
    ) -> KevyResult<()> {
        let reg = std::sync::Arc::clone(&self.indexes);
        let mut found = false;
        for g in self.guards.iter_mut() {
            let inner = &mut **g;
            sync_segs(&reg, &mut inner.idx_segs, &mut inner.store);
            if let Some((_, seg)) = inner.idx_segs.segs.iter().find(|(s, _)| s.name == name) {
                found = true;
                f(seg);
            }
        }
        if found { Ok(()) } else { Err(crate::KevyError::NotFound("no such index".into())) }
    }
}
