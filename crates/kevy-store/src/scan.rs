//! `SCAN`'s per-shard page: a bounded-work, rehash-tolerant walk over
//! this store's keyspace. Cursor arithmetic lives in
//! [`kevy_map::KevyMap::scan_step`]; this layer only applies kevy's key
//! semantics (expiry, `MATCH` glob, `TYPE` filter) and the `COUNT`
//! work bound.

use crate::{Store, glob_match, now_ns};

/// Buckets visited per [`kevy_map::KevyMap::scan_step`] call (one home
/// bucket-group). Mirrors kevy-map's group width; only used for the
/// COUNT work-bound accounting.
const BUCKETS_PER_STEP: usize = 16;

impl Store {
    /// One `SCAN` page over this shard: walk from `cursor`, visiting
    /// roughly `count` buckets (COUNT is a work bound, not a result-size
    /// promise — Redis semantics), collecting live keys that pass the
    /// optional `MATCH` glob and `TYPE` filter.
    ///
    /// Returns `(next_cursor, keys, buckets_visited)`. `next_cursor == 0`
    /// means this shard's sweep is complete. Expired-but-unreaped keys
    /// are treated as absent (no removal — same as [`Store::collect_keys`]).
    /// `type_filter` compares case-insensitively against the value's
    /// [`type name`](crate::Value::type_name); an unknown name simply
    /// never matches (Redis behaviour).
    pub fn scan_page(
        &self,
        cursor: u64,
        count: usize,
        pattern: Option<&[u8]>,
        type_filter: Option<&[u8]>,
    ) -> (u64, Vec<Vec<u8>>, usize) {
        if self.map.capacity() == 0 {
            // Never-allocated table: no buckets exist, no work was done.
            // (Distinct from "allocated but empty", which honestly costs
            // one group visit per step.)
            return (0, Vec::new(), 0);
        }
        let now = now_ns();
        let mut keys = Vec::new();
        let mut cursor = cursor;
        let mut visited = 0usize;
        let budget = count.max(1);
        loop {
            cursor = self.map.scan_step(cursor, |k, e| {
                if e.is_expired_at(now) {
                    return;
                }
                if let Some(t) = type_filter
                    && !t.eq_ignore_ascii_case(e.value.type_name().as_bytes())
                {
                    return;
                }
                if let Some(p) = pattern
                    && !glob_match(p, k.as_slice())
                {
                    return;
                }
                keys.push(k.to_vec());
            });
            visited += BUCKETS_PER_STEP;
            if cursor == 0 || visited >= budget {
                return (cursor, keys, visited);
            }
        }
    }
}
