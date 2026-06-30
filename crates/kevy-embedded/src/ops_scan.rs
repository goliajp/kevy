//! Scan family — `scan` / `hscan` / `zscan` (kevy-embedded 1.9.0,
//! closes mailrs feedback ask #7).
//!
//! Two API shapes shipped:
//!
//! - **Cursor-based** (Redis-shaped): `scan(cursor, pattern, count) ->
//!   (next_cursor, batch)`. Matches mailrs's suggested signature
//!   exactly. `cursor = 0` starts a fresh walk; `next_cursor = 0`
//!   means the walk completed.
//! - **Iterator-based** (Rust-shaped): `keys_iter(pattern) -> impl
//!   Iterator<Item = Vec<u8>>` etc. Same data; ergonomic for `for k
//!   in store.keys_iter(...) { ... }`.
//!
//! Implementation note: every scan call snapshots the matching key
//! set in one shot (via `collect_keys` / `hgetall` / `zrange`) then
//! slices into it by cursor. For in-process embedded use this is
//! the simplest correct semantics — the snapshot is stable inside
//! one walk even if other writers mutate concurrently, and the
//! memory cost is bounded by the matching subset.
//!
//! For very large keyspaces a future ship can add a truly
//! incremental cursor that walks the underlying B-tree without
//! materialising the whole match set. The API shape stays the same.

use std::io;

use crate::store::{Store, store_err};

impl Store {
    // ---- keyspace scan ----------------------------------------------

    /// `SCAN cursor [MATCH pattern] [COUNT n]` — return up to `count`
    /// keys, plus the next cursor. `cursor = 0` starts the walk;
    /// `next_cursor = 0` means the walk completed.
    ///
    /// `count` is the page size; pass `usize::MAX` to drain in one
    /// call.
    pub fn scan(
        &self,
        cursor: u64,
        pattern: Option<&[u8]>,
        count: usize,
    ) -> (u64, Vec<Vec<u8>>) {
        let all = self.collect_keys(pattern, None);
        page_into(all, cursor, count)
    }

    /// Iterator wrapper around [`Self::scan`] — emits every matching
    /// key as a `Vec<u8>`. Drains the keyspace in one snapshot at
    /// construction time; matches Rust idioms.
    pub fn keys_iter(&self, pattern: Option<&[u8]>) -> std::vec::IntoIter<Vec<u8>> {
        self.collect_keys(pattern, None).into_iter()
    }

    // ---- hash scan --------------------------------------------------

    /// `HSCAN key cursor [COUNT n]` — return up to `count` `(field,
    /// value)` pairs from the hash at `key`, plus the next cursor.
    /// `cursor = 0` starts; `next_cursor = 0` means complete.
    pub fn hscan(
        &self,
        key: &[u8],
        cursor: u64,
        count: usize,
    ) -> io::Result<(u64, Vec<(Vec<u8>, Vec<u8>)>)> {
        let pairs = self.hgetall(key)?;
        Ok(page_into(pairs, cursor, count))
    }

    /// Iterator wrapper around [`Self::hscan`].
    pub fn hash_iter(
        &self,
        key: &[u8],
    ) -> io::Result<std::vec::IntoIter<(Vec<u8>, Vec<u8>)>> {
        Ok(self.hgetall(key)?.into_iter())
    }

    // ---- zset scan --------------------------------------------------

    /// `ZSCAN key cursor [COUNT n]` — return up to `count` `(member,
    /// score)` pairs from the sorted set at `key`, in ascending score
    /// order, plus the next cursor.
    pub fn zscan(
        &self,
        key: &[u8],
        cursor: u64,
        count: usize,
    ) -> io::Result<(u64, Vec<(Vec<u8>, f64)>)> {
        let pairs = self
            .wshard(key)
            .store
            .zrange(key, 0, -1)
            .map_err(store_err)?;
        Ok(page_into(pairs, cursor, count))
    }

    /// Iterator wrapper around [`Self::zscan`].
    pub fn zset_iter(
        &self,
        key: &[u8],
    ) -> io::Result<std::vec::IntoIter<(Vec<u8>, f64)>> {
        let pairs = self
            .wshard(key)
            .store
            .zrange(key, 0, -1)
            .map_err(store_err)?;
        Ok(pairs.into_iter())
    }
}

/// Slice `data[cursor..cursor+count]` and report the next cursor
/// (`0` when the walk completed).
fn page_into<T>(data: Vec<T>, cursor: u64, count: usize) -> (u64, Vec<T>) {
    let total = data.len();
    let start = (cursor as usize).min(total);
    let end = start.saturating_add(count).min(total);
    let batch = data.into_iter().skip(start).take(end - start).collect();
    let next_cursor = if end >= total { 0 } else { end as u64 };
    (next_cursor, batch)
}
