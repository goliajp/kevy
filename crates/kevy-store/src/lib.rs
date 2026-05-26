//! kevy-store — the keyspace.
//!
//! A single-threaded, multi-type keyspace with lazy expiration. Each Redis data
//! type is backed by a modern `std` structure — behaviour-compatible, but **not**
//! Redis's legacy encodings:
//!
//! | Type | Backing structure |
//! |------|-------------------|
//! | String | `Vec<u8>` |
//! | Hash / Set | `HashMap` / `HashSet` (hashbrown Swiss table) |
//! | List | `VecDeque` (ring buffer, O(1) ends) |
//! | Sorted set | `HashMap` + `BTreeSet<(score, member)>` (a B-tree, not a skiplist) |
//!
//! Wrong-type access returns [`StoreError::WrongType`]. The API is `&mut self`
//! and lock-free, so a thread-per-core runtime ([kevy-rt]) can own one shard per
//! core with no locking. Part of the [kevy] key–value server.
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-rt]: https://crates.io/crates/kevy-rt
//!
//! # Example
//!
//! ```
//! use kevy_store::Store;
//!
//! let mut s = Store::new();
//! s.set(b"greeting", b"hello".to_vec(), None, false, false);
//! assert_eq!(s.get(b"greeting").unwrap(), Some(&b"hello"[..]));
//!
//! s.hset(b"user:1", &[(b"name".to_vec(), b"alice".to_vec())]).unwrap();
//! assert_eq!(s.hget(b"user:1", b"name").unwrap(), Some(&b"alice"[..]));
//!
//! // A string command on a hash key is a type error, as in Redis.
//! assert_eq!(s.get(b"user:1"), Err(kevy_store::StoreError::WrongType));
//! ```
#![forbid(unsafe_code)]

mod hash;
mod list;
mod set;
mod string;
mod util;
mod value;
mod zset;
pub use util::glob_match;
pub use value::*;

use kevy_hash::FxHashMap;
use std::time::{Duration, Instant};

pub(crate) struct Entry {
    pub(crate) value: Value,
    /// Absolute monotonic deadline; `None` means no expiry.
    pub(crate) expire_at: Option<Instant>,
}

/// Operation errors surfaced to the command layer.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Key holds a different type than the command expects.
    WrongType,
    /// Value is not a base-10 integer (INCR family).
    NotInteger,
    /// Result would overflow `i64`.
    Overflow,
    /// Index outside the collection (LSET).
    OutOfRange,
    /// Key does not exist where the command requires one (LSET).
    NoSuchKey,
    /// Value is not a valid float (INCRBYFLOAT).
    NotFloat,
}

/// A single-database keyspace.
///
/// The keyspace map uses [`kevy_hash::FxHasher`] rather than std's SipHash:
/// it is a single-trust-domain, single-threaded-per-shard table, so the
/// DoS-hardening tax buys nothing. Measured ~1.2–2.8× faster GET — see
/// `rfcs/2026-05-25-std-self-host-evaluation.md`.
#[derive(Default)]
pub struct Store {
    pub(crate) map: FxHashMap<Vec<u8>, Entry>,
}

impl Store {
    pub fn new() -> Self {
        Store::default()
    }

    pub(crate) fn expired(&self, key: &[u8], now: Instant) -> bool {
        match self.map.get(key) {
            Some(e) => e.expire_at.is_some_and(|t| t <= now),
            None => false,
        }
    }

    /// Drop `key` if expired; returns whether it is live afterwards.
    pub(crate) fn reap(&mut self, key: &[u8], now: Instant) -> bool {
        if self.expired(key, now) {
            self.map.remove(key);
            false
        } else {
            self.map.contains_key(key)
        }
    }

    /// Single-lookup lazy-expiring read: the live `Entry` for `key`, or `None` if
    /// absent or expired (expired keys are dropped here, as `reap` would).
    ///
    /// Two wins over the old `reap(now)`-then-`get` read path: (1) the clock is
    /// read **only when the entry actually carries a TTL** — most keys don't, so
    /// the common hit skips `Instant::now()` (~20–40 ns); (2) one fewer keyspace
    /// lookup on hits (was peek-expiry + `contains_key` + `get` = 3; now peek +
    /// `get` = 2). The two-phase shape (decide, then mutate/fetch) keeps the
    /// borrow checker happy without an owning key clone.
    pub(crate) fn live_entry(&mut self, key: &[u8]) -> Option<&Entry> {
        let expired = match self.map.get(key) {
            None => return None,
            Some(e) => matches!(e.expire_at, Some(t) if t <= Instant::now()),
        };
        if expired {
            self.map.remove(key);
            return None;
        }
        self.map.get(key)
    }

    /// Mutable [`live_entry`](Self::live_entry): the live `Entry` for `key` by
    /// `&mut`, or `None` if absent/expired (expired dropped). Same wins — clock
    /// read only on TTL'd keys, one fewer lookup than `reap`-then-`get_mut`.
    /// Read-modify commands (INCR/APPEND/…) get the entry once and mutate in
    /// place, preserving any TTL on it.
    pub(crate) fn live_entry_mut(&mut self, key: &[u8]) -> Option<&mut Entry> {
        let expired = match self.map.get(key) {
            None => return None,
            Some(e) => matches!(e.expire_at, Some(t) if t <= Instant::now()),
        };
        if expired {
            self.map.remove(key);
            return None;
        }
        self.map.get_mut(key)
    }

    // ---- generic key ops (type-agnostic) -------------------------------

    pub fn del(&mut self, keys: &[Vec<u8>]) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        for k in keys {
            if self.reap(k, now) && self.map.remove(k).is_some() {
                removed += 1;
            }
        }
        removed
    }

    pub fn exists(&mut self, keys: &[Vec<u8>]) -> usize {
        keys.iter().filter(|k| self.live_entry(k).is_some()).count()
    }

    pub fn expire(&mut self, key: &[u8], ttl: Duration) -> bool {
        let now = Instant::now();
        if !self.reap(key, now) {
            return false;
        }
        if let Some(e) = self.map.get_mut(key) {
            e.expire_at = Some(now + ttl);
            true
        } else {
            false
        }
    }

    pub fn persist(&mut self, key: &[u8]) -> bool {
        let now = Instant::now();
        if !self.reap(key, now) {
            return false;
        }
        match self.map.get_mut(key) {
            Some(e) if e.expire_at.is_some() => {
                e.expire_at = None;
                true
            }
            _ => false,
        }
    }

    /// Remaining TTL in ms: `-2` no key, `-1` no expiry, else `>= 0`.
    pub fn pttl(&mut self, key: &[u8]) -> i64 {
        let now = Instant::now();
        if !self.reap(key, now) {
            return -2;
        }
        match self.map.get(key).and_then(|e| e.expire_at) {
            None => -1,
            Some(t) => t.saturating_duration_since(now).as_millis() as i64,
        }
    }

    pub fn type_of(&mut self, key: &[u8]) -> &'static str {
        let now = Instant::now();
        if !self.reap(key, now) {
            return "none";
        }
        self.map.get(key).map_or("none", |e| e.value.type_name())
    }

    pub fn dbsize(&self) -> usize {
        self.map.len()
    }

    pub fn flush(&mut self) {
        self.map.clear();
    }

    // ---- persistence hooks ---------------------------------------------

    /// Visit every live entry as `(key, &value, ttl_ms)` for snapshotting.
    pub fn snapshot_each<F: FnMut(&[u8], &Value, Option<u64>)>(&self, mut f: F) {
        let now = Instant::now();
        for (k, e) in &self.map {
            if e.expire_at.is_some_and(|t| t <= now) {
                continue;
            }
            let ttl = e
                .expire_at
                .map(|t| t.saturating_duration_since(now).as_millis() as u64);
            f(k, &e.value, ttl);
        }
    }

    fn insert_loaded(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        self.map.insert(key, Entry { value, expire_at });
    }

    pub fn load_str(&mut self, key: Vec<u8>, value: Vec<u8>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::Str(SmallBytes::from_vec(value)), ttl_ms);
    }

    pub fn load_hash(
        &mut self,
        key: Vec<u8>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        ttl_ms: Option<u64>,
    ) {
        self.insert_loaded(key, Value::Hash(Box::new(fields.into_iter().collect())), ttl_ms);
    }

    pub fn load_list(&mut self, key: Vec<u8>, items: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::List(Box::new(items.into_iter().collect())), ttl_ms);
    }

    pub fn load_set(&mut self, key: Vec<u8>, members: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::Set(Box::new(members.into_iter().collect())), ttl_ms);
    }

    /// Collect live keys (optionally matching a glob `pattern`, up to `limit`).
    /// Used by KEYS/SCAN/RANDOMKEY. Treats expired keys as absent (no removal).
    pub fn collect_keys(&self, pattern: Option<&[u8]>, limit: Option<usize>) -> Vec<Vec<u8>> {
        let now = Instant::now();
        let mut out = Vec::new();
        for (k, e) in &self.map {
            if e.expire_at.is_some_and(|t| t <= now) {
                continue;
            }
            if let Some(p) = pattern
                && !glob_match(p, k)
            {
                continue;
            }
            out.push(k.clone());
            if limit.is_some_and(|lim| out.len() >= lim) {
                break;
            }
        }
        out
    }

    pub fn load_zset(&mut self, key: Vec<u8>, pairs: Vec<(Vec<u8>, f64)>, ttl_ms: Option<u64>) {
        let mut z = ZSetData::default();
        for (m, score) in pairs {
            z.insert(&m, score);
        }
        self.insert_loaded(key, Value::ZSet(Box::new(z)), ttl_ms);
    }
}

#[cfg(test)]
mod tests;
