//! Generic key operations + persistence hooks on [`Store`]:
//! `del`/`exists`/`expire`/`persist`/`pttl`/`type_of`/`dbsize`/`flush`/
//! `snapshot_each`/`load_*`/`collect_keys`. Type-agnostic; typed accessors
//! live in the per-type modules (string/hash/list/set/zset).
//!
//! Split out of [`crate`] for file-size hygiene.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::sync::Arc;
use core::time::Duration;

use crate::value::{HashData, SetData, Value, ZSetData};
use crate::{
    Entry, RenameOutcome, SmallBytes, Store, deadline_at, glob_match, now_ns, pack_deadline,
    remaining_ms,
};

impl Store {
    // ---- generic key ops (type-agnostic) -------------------------------

    /// `DEL` — returns the count of keys actually removed.
    pub fn del(&mut self, keys: &[&[u8]]) -> usize {
        let now = now_ns();
        let mut removed = 0;
        for k in keys {
            if self.reap(k, now) && self.remove_entry(k).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// `EXISTS` — count of live keys (duplicates count per occurrence).
    pub fn exists(&mut self, keys: &[&[u8]]) -> usize {
        keys.iter().filter(|k| self.live_entry(k).is_some()).count()
    }

    /// Set `key`'s deadline `ttl` from now. `false` if the key is not
    /// live — a key already past its own deadline is reaped first, so
    /// EXPIRE on it answers as if it were absent rather than reviving it.
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
        // A cold stub cannot leave this shard (its ColdRef names THIS
        // shard's vlog) — materialize before shipping. `remove_entry`
        // then credits nothing (the value is hot after promotion).
        if matches!(self.map.get(key).map(|e| &e.value), Some(Value::Cold(_))) {
            self.promote_in_place(key);
        }
        let entry = self.remove_entry(key)?;
        let ttl_ms = entry.expire_at_ns.map(|ns| remaining_ms(ns, now));
        Some((entry.value, ttl_ms))
    }

    /// Clone `key`'s whole entry — value plus remaining TTL — without
    /// removing it. The read half of a transaction snapshot: pair it
    /// with [`Self::put_with_ttl`] to restore, or with a delete when
    /// this returns `None` (the key did not exist).
    ///
    /// Unlike [`Self::take_with_ttl`] this leaves the entry in place,
    /// so a transaction can record the prior state on first touch and
    /// still let the closure read its own writes afterwards.
    pub fn clone_with_ttl(&mut self, key: &[u8]) -> Option<(Value, Option<u64>)> {
        let now = now_ns();
        if !self.reap(key, now) {
            return None;
        }
        let entry = self.map.get(key)?;
        let ttl_ms = entry.expire_at_ns.map(|ns| remaining_ms(ns, now));
        // Cloning a cold stub would alias its vlog record (two stubs,
        // one dead-note each — double credit). COPY-class callers get a
        // freshly materialized value instead; the original stays cold.
        if let Some(fresh) = self.tier_peek_value(key, &entry.value) {
            return Some((fresh, ttl_ms));
        }
        Some((entry.value.clone(), ttl_ms))
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
        // A seg-backed stub is keyed by its row key inside the segment
        // — the vlog's rename forward-pointer cannot express it.
        // Materialize first; the segment record strands.
        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        if let Some(e) = self.map.get(src)
            && matches!(&e.value, Value::Cold(c) if c.is_seg())
        {
            self.promote_in_place(src);
        }
        if src == dst {
            // Redis 6+ semantics: same-key rename is a no-op `+OK`.
            // (RENAMENX same-key returns `:0` per Redis since dst
            // technically already exists at src's address.)
            return if nx { RenameOutcome::DstExists } else { RenameOutcome::Renamed };
        }
        if nx {
            // Reap dst before the existence test so a TTL-expired dst
            // doesn't block the rename.
            let dst_live = self.reap(dst, now) && self.map.contains_key(dst);
            if dst_live {
                return RenameOutcome::DstExists;
            }
        }
        // Take src's entry out — keepalive form: the entry (and any
        // cold stub inside it) is re-homed intact, so RENAME moves a
        // cold key WITHOUT reading its value and without crediting its
        // record dead. Preserves TTL across rename, matching Redis.
        let Some(entry) = self.take_entry_keepalive(src) else {
            return RenameOutcome::NoSuchSrc;
        };
        // Drop any pre-existing dst (overwrite semantics). reap above
        // already handled TTL-expired dst, but the live-dst case still
        // needs removal.
        self.remove_entry(dst);
        // The record's embedded key is stale now — register the
        // forward pointer compaction resolves through (and re-account
        // the stub cost for dst's key heap bytes).
        self.tier_note_renamed(&entry.value, src, dst);
        self.insert_entry(SmallBytes::from_vec(dst.to_vec()), entry);
        RenameOutcome::Renamed
    }

    /// Drop `key`'s deadline, making it immortal. `false` if the key is
    /// not live, or was live with no deadline to drop.
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

    /// Redis's TYPE name for what `key` holds — `"none"` when it is
    /// absent or expired. `&mut` because an expired key is reaped on the
    /// way past rather than reported as its old type.
    pub fn type_of(&mut self, key: &[u8]) -> &'static str {
        let now = now_ns();
        if !self.reap(key, now) {
            return "none";
        }
        self.map.get(key).map_or("none", |e| e.value.type_name())
    }

    /// Live keys in this shard. Counts entries, not bytes, and does not
    /// reap: a key past its deadline that nothing has touched yet is still
    /// in the map and still counted here, exactly as Redis's DBSIZE
    /// behaves against lazily-expired keys.
    pub fn dbsize(&self) -> usize {
        self.map.len()
    }

    /// One arbitrary live key, drawn by probing a random slot and walking
    /// forward (wrapping once) to the first occupied, unexpired one.
    ///
    /// This used to be `collect_keys(None, Some(1))` — the first key in
    /// hash-bucket order, i.e. the same key every call until it was deleted.
    /// O(1) expected, same slight run-length bias as Redis's
    /// `dictGetRandomKey`, and the contract is "arbitrary", not "uniform".
    pub fn random_key(&mut self) -> Option<Vec<u8>> {
        let now = now_ns();
        let start = self.rng.next_u64() as usize;
        let cap = self.map.capacity();
        let start = if cap == 0 { 0 } else { start % cap };
        self.map
            .iter_from_bucket(start)
            .chain(self.map.iter().take(start))
            .find(|(_, e)| !e.is_expired_at(now))
            .map(|(k, _)| k.to_vec())
    }

    /// One raw draw from the store's random stream, for callers that need
    /// randomness OUTSIDE the store — the RANDOMKEY reducer's weighted
    /// reservoir runs on the origin shard, which must not have to invent its
    /// own entropy source to pick between candidates.
    pub fn rand_draw(&mut self) -> u64 {
        self.rng.next_u64()
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
        // Every cold stub died with the map — the whole vlog is dead,
        // and every row segment is garbage.
        self.tier_on_flushall();
        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        self.segrows_flush();
        // peak is lifetime-cumulative; intentionally not reset.
    }

    /// Count live (non-expired) keys that carry a TTL — the size of the
    /// "expire set" Redis tracks. Useful as an introspection signal for
    /// confirming the TTL subsystem actually registered keys. O(n) over the
    /// keyspace; call it for diagnostics, not on the hot path.
    pub fn ttl_pending_count(&self) -> usize {
        let now = now_ns();
        self.map.values().filter(|e| e.expire_at_ns.is_some() && !e.is_expired_at(now)).count()
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

    pub(crate) fn insert_loaded(&mut self, key: Vec<u8>, value: Value, ttl_ms: Option<u64>) {
        let expire_at = ttl_ms.map(|ms| deadline_at(now_ns(), Duration::from_millis(ms)));
        self.insert_entry(SmallBytes::from_vec(key), Entry::new(value, expire_at));
    }

    /// Install a string from a snapshot or AOF replay, re-deriving the
    /// value variant through SET's own encoding rules so a loaded key
    /// lands where a live SET of the same bytes would. See the comment
    /// inside: getting this wrong made every loaded string permanently
    /// unspillable.
    pub fn load_str(&mut self, key: Vec<u8>, value: Vec<u8>, ttl_ms: Option<u64>) {
        // Re-materialize through the SET encoding rules so a loaded
        // value lands on the exact variant a live SET of these bytes
        // would: canonical integers back to `Int` (the L2 shape the
        // snapshot serialized them from), > BULK_THRESHOLD bytes back
        // to `ArcBulk` — restoring GET's writev path AND the tiering
        // spillable class (a snapshot-loaded bulk value must be
        // demotable; the old unconditional `Value::Str` made every
        // loaded string permanently unspillable).
        let value = crate::string_set::pick_value_for_set_owned(value);
        self.insert_loaded(key, value, ttl_ms);
    }

    /// Install a hash from a snapshot or AOF replay as `(field, value)`
    /// pairs. Both sides become `SmallBytes`, so short values live in the
    /// slot rather than in their own allocation; see the comment inside for
    /// where a giant hash goes instead.
    pub fn load_hash(
        &mut self,
        key: Vec<u8>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
        ttl_ms: Option<u64>,
    ) {
        // Both field and value are SmallBytes (short values inline in the
        // slot, no per-value heap alloc). `from_vec` reuses each Vec's
        // allocation on the >22 B heap path. Giant hashes load straight
        // into buckets — same switch a live HSET applies.
        if fields.len() > crate::seg_map::HS_PROMOTE {
            let mut seg = crate::seg_map::SegMap::default();
            for (f, v) in fields {
                seg.insert(SmallBytes::from_vec(f), SmallBytes::from_vec(v));
            }
            self.insert_loaded(key, Value::SegHash(Arc::new(seg)), ttl_ms);
            return;
        }
        let hash_data: HashData = fields
            .into_iter()
            .map(|(f, v)| (SmallBytes::from_vec(f), SmallBytes::from_vec(v)))
            .collect();
        self.insert_loaded(key, Value::Hash(Arc::new(hash_data)), ttl_ms);
    }

    /// Install a list from a snapshot or AOF replay, in the given order.
    pub fn load_list(&mut self, key: Vec<u8>, items: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        // Same encoding switch a live push applies: a list past the
        // promotion threshold loads straight into segments, so a
        // snapshot restore of a giant list lands COW-ready.
        let value = if items.len() > crate::list_seg::SEG_PROMOTE {
            Value::SegList(Arc::new(crate::list_seg::SegListData::from_flat(
                items.into_iter().collect(),
            )))
        } else {
            Value::List(Arc::new(items.into_iter().collect()))
        };
        self.insert_loaded(key, value, ttl_ms);
    }

    /// Install a set from a snapshot or AOF replay. Duplicate members in
    /// the input collapse, as they would on SADD.
    pub fn load_set(&mut self, key: Vec<u8>, members: Vec<Vec<u8>>, ttl_ms: Option<u64>) {
        // Same encoding switch a live SADD applies: a giant set loads
        // straight into buckets, COW-ready.
        if members.len() > crate::seg_map::HS_PROMOTE {
            let mut seg = crate::seg_map::SegMap::default();
            for m in members {
                seg.insert(SmallBytes::from_vec(m), ());
            }
            self.insert_loaded(key, Value::SegSet(Arc::new(seg)), ttl_ms);
            return;
        }
        let set_data: SetData = members.into_iter().map(SmallBytes::from_vec).collect();
        self.insert_loaded(key, Value::Set(Arc::new(set_data)), ttl_ms);
    }

    /// Count live keys under a byte prefix and how many of them carry
    /// a TTL. O(keyspace) — a stats/ops call, not a hot-path primitive.
    pub fn prefix_stats(&self, prefix: &[u8]) -> (u64, u64) {
        let now = now_ns();
        let mut keys = 0u64;
        let mut expires = 0u64;
        for (k, e) in &self.map {
            if e.is_expired_at(now) || !k.as_slice().starts_with(prefix) {
                continue;
            }
            keys += 1;
            if e.expire_at_ns.is_some() {
                expires += 1;
            }
        }
        (keys, expires)
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

    /// Install a sorted set from a snapshot or AOF replay as
    /// `(member, score)` pairs. Order in the input does not matter — the
    /// set orders itself, as it would on ZADD.
    pub fn load_zset(&mut self, key: Vec<u8>, pairs: Vec<(Vec<u8>, f64)>, ttl_ms: Option<u64>) {
        let mut z = ZSetData::default();
        for (m, score) in pairs {
            z.insert(&m, score);
        }
        // Same encoding switch a live ZADD applies: giant zsets load
        // straight into the segmented representation, COW-ready.
        let value = if z.len() > crate::zset_seg::Z_PROMOTE {
            Value::SegZSet(Arc::new(crate::zset_seg::SegZSetData::from_flat(&z)))
        } else {
            Value::ZSet(Arc::new(z))
        };
        self.insert_loaded(key, value, ttl_ms);
    }
}
