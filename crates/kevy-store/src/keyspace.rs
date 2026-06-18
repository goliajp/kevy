//! Generic key operations + persistence hooks on [`Store`]:
//! `del`/`exists`/`expire`/`persist`/`pttl`/`type_of`/`dbsize`/`flush`/
//! `snapshot_each`/`load_*`/`collect_keys`. Type-agnostic; typed accessors
//! live in the per-type modules (string/hash/list/set/zset).
//!
//! Split out of [`crate`] for file-size hygiene.

use std::sync::Arc;
use std::time::Duration;

use crate::value::{HashData, SetData, Value, ZSetData};
use crate::{
    Entry, RenameOutcome, SmallBytes, Store, deadline_at, glob_match, now_ns, pack_deadline,
    remaining_ms,
};

impl Store {
    // ---- generic key ops (type-agnostic) -------------------------------

    pub fn del(&mut self, keys: &[Vec<u8>]) -> usize {
        let now = now_ns();
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
        let now = now_ns();
        if !self.reap(key, now) {
            return false;
        }
        let Some(e) = self.map.get_mut(key) else {
            return false;
        };
        let had = e.expire_at_ns.is_some();
        e.expire_at_ns = pack_deadline(deadline_at(now, ttl));
        let delta = i64::from(e.expire_at_ns.is_some()) - i64::from(had);
        self.adjust_expires(delta);
        true
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
        let now = now_ns();
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
            let had = e.expire_at_ns.is_some();
            e.expire_at_ns = pack_deadline(deadline_at(now, remaining));
            let delta = i64::from(e.expire_at_ns.is_some()) - i64::from(had);
            self.adjust_expires(delta);
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
        let now = now_ns();
        if !self.reap(key, now) {
            return None;
        }
        let entry = self.remove_entry(key)?;
        let ttl_ms = entry.expire_at_ns.map(|ns| remaining_ms(ns, now));
        Some((entry.value, ttl_ms))
    }

    /// Cross-shard RENAME step 2: write `value` at `key` on this
    /// shard, overwriting any prior entry. `ttl_ms` is set as a TTL
    /// relative to *now* (i.e. the orchestrator should have computed
    /// the remaining TTL on the source shard via `take_with_ttl` and
    /// is shipping that exact remaining value here).
    pub fn put_with_ttl(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| deadline_at(now_ns(), Duration::from_millis(ms)));
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
        let now = now_ns();
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
        let now = now_ns();
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
        let now = now_ns();
        if !self.reap(key, now) {
            return false;
        }
        let cleared = match self.map.get_mut(key) {
            Some(e) if e.expire_at_ns.is_some() => {
                e.expire_at_ns = None;
                true
            }
            _ => false,
        };
        if cleared {
            self.adjust_expires(-1);
        }
        cleared
    }

    /// Remaining TTL in ms: `-2` no key, `-1` no expiry, else `>= 0`.
    pub fn pttl(&mut self, key: &[u8]) -> i64 {
        let now = now_ns();
        if !self.reap(key, now) {
            return -2;
        }
        match self.map.get(key).and_then(|e| e.expire_at_ns) {
            None => -1,
            Some(ns) => remaining_ms(ns, now) as i64,
        }
    }

    pub fn type_of(&mut self, key: &[u8]) -> &'static str {
        let now = now_ns();
        if !self.reap(key, now) {
            return "none";
        }
        self.map.get(key).map_or("none", |e| e.value.type_name())
    }

    pub fn dbsize(&self) -> usize {
        self.map.len()
    }

    /// Wipe every key in this shard's keyspace (the `FLUSHALL`/`FLUSHDB`
    /// primitive). Resets `used_memory`; `used_memory_peak` is
    /// lifetime-cumulative and intentionally not reset.
    ///
    /// Named `flushall` — **not** `flush` — to avoid colliding with
    /// `Write::flush`'s "sync buffered writes to disk" meaning. This method
    /// DESTROYS data; it does not persist it.
    pub fn flushall(&mut self) {
        self.map.clear();
        self.used_memory = 0;
        self.expires = 0;
        // peak is lifetime-cumulative; intentionally not reset.
    }

    /// Deprecated alias for [`Self::flushall`]. The old name read like
    /// `Write::flush` (sync-to-disk) but actually WIPES the keyspace.
    #[deprecated(
        since = "1.17.0",
        note = "renamed to `flushall`: `flush` collides with Write::flush (sync-to-disk); this WIPES the keyspace"
    )]
    pub fn flush(&mut self) {
        self.flushall();
    }

    /// Count live (non-expired) keys that carry a TTL — the size of the
    /// "expire set" Redis tracks. Useful as an introspection signal for
    /// confirming the TTL subsystem actually registered keys. O(n) over the
    /// keyspace; call it for diagnostics, not on the hot path.
    pub fn ttl_pending_count(&self) -> usize {
        let now = now_ns();
        self.map
            .values()
            .filter(|e| e.expire_at_ns.is_some() && !e.is_expired_at(now))
            .count()
    }

    // ---- persistence hooks ---------------------------------------------

    /// Visit every live entry as `(key, &value, ttl_ms)` for snapshotting.
    pub fn snapshot_each<F: FnMut(&[u8], &Value, Option<u64>)>(&self, mut f: F) {
        let now = now_ns();
        for (k, e) in &self.map {
            if e.is_expired_at(now) {
                continue;
            }
            let ttl = e.expire_at_ns.map(|ns| remaining_ms(ns, now));
            f(k.as_slice(), &e.value, ttl);
        }
    }

    fn insert_loaded(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| deadline_at(now_ns(), Duration::from_millis(ms)));
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
        self.insert_loaded(key, Value::Hash(Arc::new(hash_data)), ttl_ms);
    }

    pub fn load_list(&mut self, key: Vec<u8>, items: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        self.insert_loaded(key, Value::List(Arc::new(items.into_iter().collect())), ttl_ms);
    }

    pub fn load_set(&mut self, key: Vec<u8>, members: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        let set_data: SetData = members.into_iter().map(SmallBytes::from_vec).collect();
        self.insert_loaded(key, Value::Set(Arc::new(set_data)), ttl_ms);
    }

    /// Collect live keys (optionally matching a glob `pattern`, up to `limit`).
    /// Used by KEYS/SCAN/RANDOMKEY. Treats expired keys as absent (no removal).
    pub fn collect_keys(&self, pattern: Option<&[u8]>, limit: Option<usize>) -> Vec<Vec<u8>> {
        let now = now_ns();
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
        self.insert_loaded(key, Value::ZSet(Arc::new(z)), ttl_ms);
    }

    /// Insert one already-typed `(key, value, ttl)` triple, e.g. straight out
    /// of another store's [`Self::snapshot_each`] — the redistribution step
    /// both reshard paths (embedded `shards` bring-up, server routing
    /// migration) use to re-home keys after a layout change.
    pub fn load_value(&mut self, key: &[u8], value: &Value, ttl_ms: Option<u64>) {
        let k = key.to_vec();
        match value {
            Value::Str(v) => self.load_str(k, v.to_vec(), ttl_ms),
            Value::Hash(h) => self.load_hash(
                k,
                h.iter().map(|(f, v)| (f.to_vec(), v.clone())).collect(),
                ttl_ms,
            ),
            Value::List(l) => self.load_list(k, l.iter().cloned().collect(), ttl_ms),
            Value::Set(s) => self.load_set(k, s.iter().map(kevy_bytes::SmallBytes::to_vec).collect(), ttl_ms),
            Value::ZSet(z) => self.load_zset(
                k,
                z.ordered().map(|(m, sc)| (m.to_vec(), sc)).collect(),
                ttl_ms,
            ),
            Value::Stream(st) => {
                let entries: Vec<crate::stream::LoadedStreamEntry> = st
                    .iter_entries()
                    .map(|(id, fv)| {
                        let fvv = fv
                            .iter()
                            .map(|(f, v)| (f.as_slice().to_vec(), v.as_slice().to_vec()))
                            .collect();
                        (id.ms, id.seq, fvv)
                    })
                    .collect();
                let last = st.last_id();
                let mxd = st.max_deleted_id();
                self.load_stream(
                    k,
                    entries,
                    (last.ms, last.seq),
                    (mxd.ms, mxd.seq),
                    st.entries_added(),
                    st.export_groups(),
                    ttl_ms,
                );
            }
        }
    }

    /// Snapshot-load a stream: every entry plus the per-stream scalar
    /// state (last_id, max_deleted_id, entries_added) and the consumer
    /// groups are restored verbatim. Caller passes already-decoded
    /// primitive tuples; this fn does the [`SmallBytes`] /
    /// [`crate::StreamData`] conversion.
    #[allow(clippy::too_many_arguments)]
    pub fn load_stream(
        &mut self,
        key: Vec<u8>,
        entries: Vec<crate::stream::LoadedStreamEntry>,
        last_id: (u64, u64),
        max_deleted_id: (u64, u64),
        entries_added: u64,
        groups: Vec<crate::stream::LoadedGroup>,
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
        s.import_groups(groups);
        self.insert_loaded(key, Value::Stream(Arc::new(s)), ttl_ms);
    }
}
