//! Cursor-based and iterator-based scanning: `scan` / `hscan` /
//! `zscan` plus the `keys_iter` / `hash_iter` / `zset_iter` adapters.
//!
//! Two API shapes:
//!
//! - **Cursor-based** (Redis-shaped): `scan(cursor, pattern, count) ->
//!   (next_cursor, batch)`. A `cursor` of `0` starts a fresh walk; a
//!   returned `next_cursor` of `0` means the walk completed.
//! - **Iterator-based** (Rust-shaped): `keys_iter(pattern) -> impl
//!   Iterator<Item = Vec<u8>>`, etc.
//!
//! The keyspace walk has the server's `SCAN` contract: a key present for
//! the whole walk is returned at least once, and a page holds what it
//! walked rather than a snapshot of the keyspace. `hscan` / `zscan` read
//! the one collection they name and slice it by cursor.

use crate::KevyResult;

use crate::store::{Store, store_err};
use crate::store_glue::lock_read;

/// Bits of a keyspace cursor that carry the position within a shard; the
/// shard index sits above them (the server's layout).
const SCAN_POS_BITS: u32 = 54;
const SCAN_POS_MASK: u64 = (1 << SCAN_POS_BITS) - 1;
/// Keys a [`KeysIter`] asks for per page.
const ITER_PAGE: usize = 256;

/// The iterator [`Store::keys_iter`] returns: the keys matching its
/// pattern, a page at a time.
///
/// ```
/// # use kevy_embedded::{Config, Store};
/// let s = Store::open(Config::default())?;
/// s.set(b"user:1", b"x")?;
/// s.set(b"order:1", b"y")?;
/// let users: Vec<Vec<u8>> = s.keys_iter(Some(b"user:*")).collect();
/// assert_eq!(users, [b"user:1".to_vec()]);
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
pub struct KeysIter<'a> {
    store: &'a Store,
    pattern: Option<Vec<u8>>,
    cursor: u64,
    page: std::vec::IntoIter<Vec<u8>>,
    done: bool,
}

impl std::fmt::Debug for KeysIter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeysIter")
            .field("pattern", &self.pattern)
            .field("cursor", &self.cursor)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl Iterator for KeysIter<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(k) = self.page.next() {
                return Some(k);
            }
            if self.done {
                return None;
            }
            let (next, keys) = self.store.scan(self.cursor, self.pattern.as_deref(), ITER_PAGE);
            (self.cursor, self.done) = (next, next == 0);
            self.page = keys.into_iter();
        }
    }
}

/// One `HSCAN`/`ZSCAN`-style page: `(next_cursor, items)` where each
/// item is a `(member, value-or-score)` pair.
type PairPage = (u64, Vec<(Vec<u8>, Vec<u8>)>);

/// One `ZSCAN` page: `(next_cursor, (member, score) pairs)`.
type ScorePage = (u64, Vec<(Vec<u8>, f64)>);

impl Store {
    // ---- keyspace scan ----------------------------------------------

    /// `SCAN cursor [MATCH pattern] [COUNT n]` — keys and the next cursor.
    /// `cursor = 0` starts the walk; `next_cursor = 0` means it completed.
    ///
    /// As in Redis, `count` is how much of the keyspace a page walks, not
    /// a promise of how many keys it holds: a page returns at least
    /// `count` keys unless the walk ends first, and may return more. A key
    /// present for the whole walk is returned at least once; one written
    /// meanwhile may come back twice. `usize::MAX` drains in one call. The
    /// cursor names a shard and a position in it, so a page costs what it
    /// walks, not the whole keyspace.
    ///
    /// ```
    /// # use kevy_embedded::{Config, Store};
    /// let s = Store::open(Config::default())?;
    /// for i in 0..50 {
    ///     s.set(format!("k{i}").as_bytes(), b"v")?;
    /// }
    /// let (mut cursor, mut seen) = (0, std::collections::BTreeSet::new());
    /// loop {
    ///     let (next, keys) = s.scan(cursor, Some(b"k*"), 10);
    ///     seen.extend(keys);
    ///     if next == 0 {
    ///         break;
    ///     }
    ///     cursor = next;
    /// }
    /// assert_eq!(seen.len(), 50);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub fn scan(&self, cursor: u64, pattern: Option<&[u8]>, count: usize) -> (u64, Vec<Vec<u8>>) {
        if count == 0 {
            return (0, Vec::new());
        }
        let (mut shard, mut pos) = ((cursor >> SCAN_POS_BITS) as usize, cursor & SCAN_POS_MASK);
        let mut keys = Vec::new();
        while shard < self.shards.len() {
            let want = count.saturating_sub(keys.len());
            let g = lock_read(&self.shards[shard]);
            let (next, mut page, _) = g.store.scan_page(pos, want, pattern, None);
            drop(g);
            keys.append(&mut page);
            if next == 0 {
                (shard, pos) = (shard + 1, 0);
            } else {
                pos = next;
            }
            if keys.len() >= count && shard < self.shards.len() {
                return ((shard as u64) << SCAN_POS_BITS | pos, keys);
            }
        }
        (0, keys)
    }

    /// Every key matching `pattern`, walked a page at a time: the iterator
    /// holds one page, not the keyspace. Same contract as [`Self::scan`] —
    /// a key present throughout is yielded at least once.
    ///
    /// ```
    /// # use kevy_embedded::{Config, Store};
    /// let s = Store::open(Config::default())?;
    /// s.set(b"a", b"1")?;
    /// s.set(b"b", b"2")?;
    /// let mut keys: Vec<Vec<u8>> = s.keys_iter(None).collect();
    /// keys.sort();
    /// assert_eq!(keys, [b"a".to_vec(), b"b".to_vec()]);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub fn keys_iter(&self, pattern: Option<&[u8]>) -> KeysIter<'_> {
        KeysIter {
            store: self,
            pattern: pattern.map(<[u8]>::to_vec),
            cursor: 0,
            page: Vec::new().into_iter(),
            done: false,
        }
    }

    // ---- hash scan --------------------------------------------------

    /// `HSCAN key cursor [COUNT n]` — return up to `count` `(field,
    /// value)` pairs from the hash at `key`, plus the next cursor.
    /// `cursor = 0` starts; `next_cursor = 0` means complete.
    pub fn hscan(&self, key: &[u8], cursor: u64, count: usize) -> KevyResult<PairPage> {
        let pairs = self.hgetall(key)?;
        Ok(page_into(pairs, cursor, count))
    }

    /// Iterator wrapper around [`Self::hscan`].
    pub fn hash_iter(&self, key: &[u8]) -> KevyResult<std::vec::IntoIter<(Vec<u8>, Vec<u8>)>> {
        Ok(self.hgetall(key)?.into_iter())
    }

    // ---- zset scan --------------------------------------------------

    /// `ZSCAN key cursor [COUNT n]` — return up to `count` `(member,
    /// score)` pairs from the sorted set at `key`, in ascending score
    /// order, plus the next cursor.
    pub fn zscan(&self, key: &[u8], cursor: u64, count: usize) -> KevyResult<ScorePage> {
        let pairs = self.wshard(key).store.zrange(key, 0, -1).map_err(store_err)?;
        Ok(page_into(pairs, cursor, count))
    }

    /// Iterator wrapper around [`Self::zscan`].
    pub fn zset_iter(&self, key: &[u8]) -> KevyResult<std::vec::IntoIter<(Vec<u8>, f64)>> {
        let pairs = self.wshard(key).store.zrange(key, 0, -1).map_err(store_err)?;
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
