//! Generic key operations + persistence hooks on [`Store`]:
//! `del`/`exists`/`expire`/`persist`/`pttl`/`type_of`/`dbsize`/`flush`/
//! `snapshot_each`/`load_*`/`collect_keys`. Type-agnostic; typed accessors
//! live in the per-type modules (string/hash/list/set/zset).
//!
//! Split out of [`crate`] for file-size hygiene.

use std::time::{Duration, Instant};

use crate::value::{HashData, SetData, Value, ZSetData};
use crate::{Entry, SmallBytes, Store, glob_match, pack_deadline, unpack_deadline};

impl Store {
    // ---- generic key ops (type-agnostic) -------------------------------

    pub fn del(&mut self, keys: &[Vec<u8>]) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        for k in keys {
            if self.reap(k, now) && self.remove_entry(k.as_slice()).is_some() {
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
            e.expire_at_ns = pack_deadline(now + ttl);
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
            Some(e) if e.expire_at_ns.is_some() => {
                e.expire_at_ns = None;
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
        match self.map.get(key).and_then(|e| e.expire_at_ns) {
            None => -1,
            Some(ns) => unpack_deadline(ns)
                .saturating_duration_since(now)
                .as_millis() as i64,
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
        self.used_memory = 0;
        // peak is lifetime-cumulative; intentionally not reset.
    }

    // ---- persistence hooks ---------------------------------------------

    /// Visit every live entry as `(key, &value, ttl_ms)` for snapshotting.
    pub fn snapshot_each<F: FnMut(&[u8], &Value, Option<u64>)>(&self, mut f: F) {
        let now = Instant::now();
        for (k, e) in &self.map {
            if e.is_expired_at(now) {
                continue;
            }
            let ttl = e
                .expire_at_ns
                .map(|ns| unpack_deadline(ns).saturating_duration_since(now).as_millis() as u64);
            f(k.as_slice(), &e.value, ttl);
        }
    }

    fn insert_loaded(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        self.insert_entry(SmallBytes::from_vec(key), Entry::new(value, expire_at));
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
        // Hash keys are SmallBytes; values stay Vec<u8>. From-iter converts.
        let hash_data: HashData = fields
            .into_iter()
            .map(|(f, v)| (SmallBytes::from_vec(f), v))
            .collect();
        self.insert_loaded(key, Value::Hash(Box::new(hash_data)), ttl_ms);
    }

    pub fn load_list(&mut self, key: Vec<u8>, items: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::List(Box::new(items.into_iter().collect())), ttl_ms);
    }

    pub fn load_set(&mut self, key: Vec<u8>, members: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        let set_data: SetData = members.into_iter().map(SmallBytes::from_vec).collect();
        self.insert_loaded(key, Value::Set(Box::new(set_data)), ttl_ms);
    }

    /// Collect live keys (optionally matching a glob `pattern`, up to `limit`).
    /// Used by KEYS/SCAN/RANDOMKEY. Treats expired keys as absent (no removal).
    pub fn collect_keys(&self, pattern: Option<&[u8]>, limit: Option<usize>) -> Vec<Vec<u8>> {
        let now = Instant::now();
        let mut out = Vec::new();
        for (k, e) in &self.map {
            if e.is_expired_at(now) {
                continue;
            }
            if let Some(p) = pattern
                && !glob_match(p, k.as_slice())
            {
                continue;
            }
            out.push(k.to_vec());
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
