//! A resumable walk over the keys under one prefix, a batch at a time.
//!
//! The backfills a declaration starts used to copy every key under the
//! prefix up front: about 72 bytes a key on the heap, three copies per
//! table, freed only as each backfill finished. The walk keeps a cursor
//! instead, so it holds one batch.
//!
//! [`Store::walk_page`]'s contract is what makes that safe beside live
//! writes: a key present for the whole walk is returned at least once. A
//! key written after the walk began is not promised a visit and needs none,
//! since the write hook has already applied it. A key returned twice (a
//! table growth restarts the walk) is applied twice, which every backfill
//! tolerates. The walk goes in storage order, as the key copy did, so each
//! batch's rows are looked up again from neighbouring buckets.

use kevy_store::Store;

/// Buckets visited per page: a batch overshoots its size by at most one
/// page of keys.
const STEP_BUCKETS: usize = 256;

#[derive(Debug)]
pub(crate) struct KeyWalk {
    /// `prefix*`, the glob the backfills have always matched keys with.
    pattern: Vec<u8>,
    cursor: u64,
    done: bool,
}

impl KeyWalk {
    pub(crate) fn new(prefix: &[u8]) -> Self {
        let mut pattern = prefix.to_vec();
        pattern.push(b'*');
        KeyWalk { pattern, cursor: 0, done: false }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// The next keys under the prefix: at least `want` of them unless the
    /// walk ends first, which leaves [`Self::is_done`] true.
    pub(crate) fn next_batch(&mut self, store: &Store, want: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while !self.done && out.len() < want {
            let (next, mut keys) = store.walk_page(self.cursor, STEP_BUCKETS, Some(&self.pattern));
            if out.is_empty() {
                out = keys;
            } else {
                out.append(&mut keys);
            }
            self.cursor = next;
            self.done = next == 0;
        }
        out
    }
}
