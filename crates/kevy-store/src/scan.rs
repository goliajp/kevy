//! `SCAN`'s per-shard page: a bounded-work, rehash-tolerant walk over
//! this store's keyspace. Cursor arithmetic lives in
//! [`kevy_map::KevyMap::scan_step`]; this layer only applies kevy's key
//! semantics (expiry, `MATCH` glob, `TYPE` filter) and the `COUNT`
//! work bound.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
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

    /// One page of a walk over the keyspace in storage order: the live keys
    /// in the next `buckets` buckets that match the optional glob
    /// `pattern`, and the cursor to continue from (`0` both starts and ends
    /// the walk, as with [`Store::scan_page`]).
    ///
    /// Storage order is [`Store::collect_keys`]'s, so a page's keys sit
    /// next to each other in the table and a caller that looks each one up
    /// again finds it in cache. Between pages the keyspace may change: a
    /// key present for the whole walk is returned at least once. A growth
    /// of the table between two pages restarts the walk from the beginning,
    /// so keys may come back again; [`Store::scan_page`] never restarts,
    /// but hashes every key it visits to stay in step with a growth.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// for i in 0..500 {
    ///     s.set(format!("row:{i}").as_bytes(), b"v".to_vec(), None, kevy_store::SetCondition::Always);
    /// }
    /// s.set(b"other", b"v".to_vec(), None, kevy_store::SetCondition::Always);
    /// let (mut rows, mut cursor) = (Vec::new(), 0);
    /// loop {
    ///     let (next, keys) = s.walk_page(cursor, 64, Some(b"row:*"));
    ///     rows.extend(keys);
    ///     cursor = next;
    ///     if cursor == 0 {
    ///         break;
    ///     }
    /// }
    /// assert_eq!(rows.len(), 500);
    /// ```
    pub fn walk_page(
        &self,
        cursor: u64,
        buckets: usize,
        pattern: Option<&[u8]>,
    ) -> (u64, Vec<Vec<u8>>) {
        let cap = self.map.capacity();
        if cap == 0 {
            return (0, Vec::new());
        }
        // the cursor carries the capacity it was minted against beside the
        // next bucket, so a growth in between is seen and the walk restarts
        let start = match walk_cursor_parts(cursor) {
            Some((minted_cap, bucket)) if minted_cap == cap => bucket,
            _ => 0,
        };
        let now = now_ns();
        let mut keys = Vec::new();
        let next = self.map.scan_buckets(start, buckets.max(1), |k, e| {
            if e.is_expired_at(now) {
                return;
            }
            if pattern.is_none_or(|p| glob_match(p, k.as_slice())) {
                keys.push(k.to_vec());
            }
        });
        if next >= cap {
            return (0, keys);
        }
        ((u64::from(cap.trailing_zeros()) << WALK_CAP_SHIFT) | next as u64, keys)
    }
}

/// Where a walk cursor keeps the capacity's log2; the bucket sits below it.
const WALK_CAP_SHIFT: u32 = 56;

/// `(capacity, next bucket)` out of a walk cursor; `None` for `0`, the start.
fn walk_cursor_parts(cursor: u64) -> Option<(usize, usize)> {
    if cursor == 0 {
        return None;
    }
    let bucket = (cursor & ((1 << WALK_CAP_SHIFT) - 1)) as usize;
    let cap = 1usize.checked_shl((cursor >> WALK_CAP_SHIFT) as u32)?;
    Some((cap, bucket))
}
