//! Internal accounting helpers on [`Store`]: per-entry weight bookkeeping,
//! LRU/LFU clock advance, prefetch, and the lazy-expire `live_entry` /
//! `live_entry_mut` lookups used by every typed accessor.
//!
//! Split out of [`crate`] for file-size hygiene. Nothing here is part of
//! the public surface — all methods are `pub(crate)` and called by sibling
//! modules (string/hash/list/set/zset/evict/expire/keyspace).

use kevy_hash::KevyHash;

use crate::value::ENTRY_OVERHEAD;
use crate::{Entry, SmallBytes, Store, apply_delta, evict, key_heap_bytes_for};

impl Store {
    /// Insert a fresh entry, replacing any prior. Stamps `entry.weight` from
    /// the live value and key, then updates `used_memory` for either the
    /// new-key (charges [`ENTRY_OVERHEAD`]) or overwrite (weight swap) case.
    pub(crate) fn insert_entry(&mut self, key: SmallBytes, mut entry: Entry) -> Option<Entry> {
        entry.set_weight(key.heap_bytes() as u64 + entry.value.weight());
        if self.maxmemory > 0 {
            self.tick_clock();
            entry.set_lru_clock(self.clock_counter as u32);
        }
        let new_w = entry.weight();
        let new_has_ttl = entry.expire_at_ns.is_some();
        let prev = self.map.insert(key, entry);
        match &prev {
            Some(old) => {
                self.used_memory = self
                    .used_memory
                    .saturating_sub(old.weight())
                    .saturating_add(new_w);
            }
            None => {
                self.used_memory = self.used_memory.saturating_add(new_w + ENTRY_OVERHEAD);
            }
        }
        let old_has_ttl = prev.as_ref().is_some_and(|o| o.expire_at_ns.is_some());
        self.adjust_expires(i64::from(new_has_ttl) - i64::from(old_has_ttl));
        self.update_peak();
        prev
    }

    /// Remove a key, returning the displaced entry (`None` if absent).
    /// Frees the entry's cached weight + [`ENTRY_OVERHEAD`].
    pub(crate) fn remove_entry(&mut self, key: &[u8]) -> Option<Entry> {
        let old = self.map.remove(key)?;
        self.used_memory = self
            .used_memory
            .saturating_sub(old.weight() + ENTRY_OVERHEAD);
        if old.expire_at_ns.is_some() {
            self.adjust_expires(-1);
        }
        Some(old)
    }

    /// Apply a signed weight delta to `key`'s cached `Entry::weight` AND to
    /// the shard-wide `used_memory`. Used by in-place collection mutators
    /// (HSET adding a field, LPUSH adding an item, …) so we account in O(1)
    /// without re-walking the container.
    pub(crate) fn account_delta(&mut self, key: &[u8], delta: i64) {
        if delta == 0 {
            return;
        }
        if let Some(e) = self.map.get_mut(key) {
            e.add_to_weight(delta);
        }
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Recompute `weight` for the entry at `key` from its current value +
    /// key, then propagate the delta to `used_memory`. Use after a wholesale
    /// in-place value swap (SET / APPEND / INCRBYFLOAT) where the prior
    /// `Value`'s weight was already cached on the entry.
    pub(crate) fn reweigh_entry(&mut self, key: &[u8]) {
        let key_heap = key_heap_bytes_for(key);
        let Some(e) = self.map.get_mut(key) else {
            return;
        };
        let new_w = key_heap + e.value.weight();
        let delta = new_w as i64 - e.weight() as i64;
        e.set_weight(new_w);
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Advance the global access ordinal by one tick. Only invoked under
    /// `maxmemory > 0` so the wrapping_add cost stays out of the unlimited
    /// fast path.
    #[inline]
    pub(crate) fn tick_clock(&mut self) {
        self.clock_counter = self.clock_counter.wrapping_add(1);
    }

    #[inline]
    fn update_peak(&mut self) {
        if self.used_memory > self.used_memory_peak {
            self.used_memory_peak = self.used_memory;
        }
    }

    /// Apply a weight delta computed in-place by a caller that already held
    /// `&mut Entry` (overwrite-SET fast path) — same arithmetic as
    /// [`Self::reweigh_entry`] but WITHOUT the second hash + map probe that
    /// `reweigh_entry(key)` pays to re-find the entry it just mutated.
    #[inline]
    pub(crate) fn apply_weight_delta(&mut self, delta: i64) {
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Hint the CPU to fetch the bucket cache line for `key` into L1. Called
    /// by the reactor's parse loop on command N+1 while command N is still
    /// being dispatched — by the time N+1 actually probes the table, the
    /// metadata line is hot. No-op when the table is empty. Cheap when not.
    #[inline]
    pub fn prefetch_for_key(&self, key: &[u8]) {
        let hash = key.kevy_hash();
        self.map.prefetch_for_hash(hash);
    }

    pub(crate) fn expired(&self, key: &[u8], now: u64) -> bool {
        match self.map.get(key) {
            Some(e) => e.is_expired_at(now),
            None => false,
        }
    }

    /// Drop `key` if expired; returns whether it is live afterwards. `now` is
    /// monotonic ns since epoch (from [`crate::now_ns`]).
    pub(crate) fn reap(&mut self, key: &[u8], now: u64) -> bool {
        if self.expired(key, now) {
            self.remove_entry(key);
            self.expired_keys_total = self.expired_keys_total.saturating_add(1);
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
        // G-A4 (v1.25): TTL-free fast path. Read cached clock fields
        // ONLY when the entry actually carries a TTL — most keys don't,
        // and the prior implementation paid two field reads + a pass
        // through `is_expired` (which itself short-circuits on None)
        // unconditionally. Saves ~5 ns / hot lookup across every
        // collection / string read path.
        let needs_check = match self.map.get(key) {
            None => return None,
            Some(e) => e.expire_at_ns.is_some(),
        };
        if needs_check {
            let (uc, cn) = (self.cached_clock, self.cached_ns);
            let expired = self.map.get(key).is_some_and(|e| e.is_expired(uc, cn));
            if expired {
                self.remove_entry(key);
                self.expired_keys_total = self.expired_keys_total.saturating_add(1);
                return None;
            }
        }
        if self.maxmemory > 0 {
            self.tick_clock();
            let c = self.clock_counter as u32;
            let e = self.map.get_mut(key)?;
            evict::touch_on_access(e, self.eviction_policy, c);
            return Some(&*e);
        }
        self.map.get(key)
    }

    /// Mutable [`live_entry`](Self::live_entry): the live `Entry` for `key` by
    /// `&mut`, or `None` if absent/expired (expired dropped). Same wins — clock
    /// read only on TTL'd keys, one fewer lookup than `reap`-then-`get_mut`.
    /// Read-modify commands (INCR/APPEND/…) get the entry once and mutate in
    /// place, preserving any TTL on it.
    pub(crate) fn live_entry_mut(&mut self, key: &[u8]) -> Option<&mut Entry> {
        // G-A4 (v1.25): see `live_entry` doc — TTL-free fast path.
        let needs_check = match self.map.get(key) {
            None => return None,
            Some(e) => e.expire_at_ns.is_some(),
        };
        if needs_check {
            let (uc, cn) = (self.cached_clock, self.cached_ns);
            let expired = self.map.get(key).is_some_and(|e| e.is_expired(uc, cn));
            if expired {
                self.remove_entry(key);
                self.expired_keys_total = self.expired_keys_total.saturating_add(1);
                return None;
            }
        }
        if self.maxmemory > 0 {
            self.tick_clock();
            let c = self.clock_counter as u32;
            let e = self.map.get_mut(key)?;
            evict::touch_on_access(e, self.eviction_policy, c);
            return Some(e);
        }
        self.map.get_mut(key)
    }
}
