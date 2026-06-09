//! Generic key operations + persistence hooks on [`Store`]:
//! `del`/`exists`/`expire`/`persist`/`pttl`/`type_of`/`dbsize`/`flush`/
//! `snapshot_each`/`load_*`/`collect_keys`. Type-agnostic; typed accessors
//! live in the per-type modules (string/hash/list/set/zset).
//!
//! Split out of [`crate`] for file-size hygiene.

use std::time::{Duration, Instant};

use crate::value::{HashData, SetData, Value, ZSetData};
use crate::{Entry, RenameOutcome, SmallBytes, Store, glob_match, pack_deadline, unpack_deadline};

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

    /// `EXPIREAT`/`PEXPIREAT` semantics: set an **absolute** wall-clock
    /// deadline (Unix epoch millis). This is the persistence-safe form —
    /// a deadline survives restart unchanged, unlike the relative
    /// [`Self::expire`] (whose duration is re-anchored to "now"). A
    /// deadline already in the past deletes the key immediately (Redis
    /// behaviour). Returns `true` iff the key existed (and was either
    /// re-dated or deleted). The wall-clock → monotonic-`Instant`
    /// conversion happens here so callers persist absolute time but the
    /// hot path keeps its cheap monotonic deadline.
    pub fn expire_at_unix_ms(&mut self, key: &[u8], deadline_ms: u64) -> bool {
        let now = Instant::now();
        if !self.reap(key, now) || !self.map.contains_key(key) {
            return false;
        }
        let wall_now = crate::now_unix_ms();
        if deadline_ms <= wall_now {
            // Past deadline: delete now, just like Redis EXPIREAT in the past.
            self.remove_entry(key);
            return true;
        }
        let remaining = Duration::from_millis(deadline_ms - wall_now);
        if let Some(e) = self.map.get_mut(key) {
            e.expire_at_ns = pack_deadline(now + remaining);
        }
        true
    }

    /// Cross-shard RENAME step 1: atomically remove the entry at
    /// `key` (if any), returning the `(value, ttl_ms_remaining)`. The
    /// orchestrator on the origin shard ships the result into a
    /// follow-up [`Self::put_with_ttl`] on the destination shard.
    /// Lazy-reaps an expired entry before the take (so an expired
    /// key is observed as `None`, not silently rehomed).
    pub fn take_with_ttl(&mut self, key: &[u8]) -> Option<(Value, Option<u64>)> {
        let now = Instant::now();
        if !self.reap(key, now) {
            return None;
        }
        let entry = self.remove_entry(key)?;
        let ttl_ms = entry.expire_at_ns.map(|ns| {
            unpack_deadline(ns).saturating_duration_since(now).as_millis() as u64
        });
        Some((entry.value, ttl_ms))
    }

    /// Cross-shard RENAME step 2: write `value` at `key` on this
    /// shard, overwriting any prior entry. `ttl_ms` is set as a TTL
    /// relative to *now* (i.e. the orchestrator should have computed
    /// the remaining TTL on the source shard via `take_with_ttl` and
    /// is shipping that exact remaining value here).
    pub fn put_with_ttl(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let entry = Entry::new(value, expire_at);
        // Overwrite — drop any existing entry first so the accounting
        // doesn't double-count.
        self.remove_entry(&key);
        self.insert_entry(SmallBytes::from_vec(key), entry);
    }

    /// Whether a live (non-expired) entry exists at `key`. Reaps an
    /// expired entry as a side effect. Used by the cross-shard RENAME
    /// orchestrator's `nx` pre-check.
    pub fn key_exists(&mut self, key: &[u8]) -> bool {
        let now = Instant::now();
        self.reap(key, now) && self.map.contains_key(key)
    }

    /// `RENAME` (or `RENAMENX` if `nx`). Atomic on this shard. Returns
    /// the outcome so the dispatch layer can emit the right RESP frame
    /// (RENAME: `+OK` or `-ERR no such key`; RENAMENX: `:1`/`:0`/error).
    ///
    /// Cross-shard rename is the runtime's job — by the time this is
    /// called, both `src` and `dst` are guaranteed to live on the same
    /// shard. See `kevy-rt::start_rename` for the cross-shard split.
    pub fn rename(&mut self, src: &[u8], dst: &[u8], nx: bool) -> RenameOutcome {
        let now = Instant::now();
        if !self.reap(src, now) {
            return RenameOutcome::NoSuchSrc;
        }
        if src == dst {
            // Redis 6+ semantics: same-key rename is a no-op `+OK`.
            // (RENAMENX same-key returns `:0` per Redis since dst
            // technically already exists at src's address.)
            return if nx {
                RenameOutcome::DstExists
            } else {
                RenameOutcome::Renamed
            };
        }
        if nx {
            // Reap dst before the existence test so a TTL-expired dst
            // doesn't block the rename.
            let dst_live = self.reap(dst, now) && self.map.contains_key(dst);
            if dst_live {
                return RenameOutcome::DstExists;
            }
        }
        // Take src's entry out. `remove_entry` returns the full Entry
        // (value + TTL) — preserves TTL across rename, matching Redis.
        let Some(entry) = self.remove_entry(src) else {
            return RenameOutcome::NoSuchSrc;
        };
        // Drop any pre-existing dst (overwrite semantics). reap above
        // already handled TTL-expired dst, but the live-dst case still
        // needs removal.
        self.remove_entry(dst);
        self.insert_entry(SmallBytes::from_vec(dst.to_vec()), entry);
        RenameOutcome::Renamed
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

    /// Snapshot-load a stream: every entry plus the per-stream scalar
    /// state (last_id, max_deleted_id, entries_added) is restored
    /// verbatim. Caller passes already-decoded primitive tuples; this
    /// fn does the [`SmallBytes`] / [`crate::StreamData`] conversion.
    pub fn load_stream(
        &mut self,
        key: Vec<u8>,
        entries: Vec<crate::stream::LoadedStreamEntry>,
        last_id: (u64, u64),
        max_deleted_id: (u64, u64),
        entries_added: u64,
        ttl_ms: Option<u64>,
    ) {
        let mut s = crate::stream::StreamData::default();
        for (ms, seq, fv) in entries {
            let id = crate::stream::StreamId { ms, seq };
            let fv_small: Vec<(SmallBytes, SmallBytes)> = fv
                .into_iter()
                .map(|(f, v)| (SmallBytes::from_vec(f), SmallBytes::from_vec(v)))
                .collect();
            s.load_entry(id, fv_small);
        }
        s.set_loaded_state(
            crate::stream::StreamId { ms: last_id.0, seq: last_id.1 },
            crate::stream::StreamId { ms: max_deleted_id.0, seq: max_deleted_id.1 },
            entries_added,
        );
        self.insert_loaded(key, Value::Stream(Box::new(s)), ttl_ms);
    }
}
