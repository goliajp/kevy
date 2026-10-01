//! Internal accounting helpers on [`Store`]: per-entry weight bookkeeping,
//! LRU/LFU clock advance, prefetch, and the lazy-expire `live_entry` /
//! `live_entry_mut` lookups used by every typed accessor.
//!
//! Split out of [`crate`] for file-size hygiene. Nothing here is part of
//! the public surface — all methods are `pub(crate)` and called by sibling
//! modules (string/hash/list/set/zset/evict/expire/keyspace).

use kevy_hash::KevyHash;

use crate::{Entry, SmallBytes, Store, apply_delta, evict, key_heap_bytes_for};

impl Store {
    /// Insert a fresh entry, replacing any prior. Stamps `entry.weight` from
    /// the live value and key, then updates `used_memory` by the weight swap
    /// and by whatever the keyspace table grew to make room.
    pub(crate) fn insert_entry(&mut self, key: SmallBytes, mut entry: Entry) -> Option<Entry> {
        // New-key event capture: the owned key copy is only paid when
        // the capture flag is on (server with `n` notifications).
        let new_key_copy =
            (self.notify_capture & crate::notify::CAPTURE_NEW != 0).then(|| key.to_vec());
        // A wholesale value replacement (type change / RESTORE)
        // discards any per-field hash TTLs; a fresh create is a no-op.
        self.clear_hash_key_ttls(key.as_slice());
        let key_heap = key.heap_bytes() as u64;
        entry.set_weight(key_heap + entry.value.weight());
        if self.clock_on() {
            self.tick_clock();
            entry.set_lru_clock(self.clock_counter as u32);
        }
        let new_w = entry.weight();
        let new_has_ttl = entry.expire_at_ns.is_some();
        let cap = self.map.capacity();
        let prev = self.map.insert(key, entry);
        if self.map.capacity() != cap {
            self.charge_keyspace_growth();
        }
        match &prev {
            Some(old) => {
                // A displaced cold stub's vlog record dies with it.
                self.tier_note_dead(key_heap, &old.value);
                self.used_memory =
                    self.used_memory.saturating_sub(old.weight()).saturating_add(new_w);
            }
            None => self.used_memory = self.used_memory.saturating_add(new_w),
        }
        let old_has_ttl = prev.as_ref().is_some_and(|o| o.expire_at_ns.is_some());
        self.adjust_expires(i64::from(new_has_ttl) - i64::from(old_has_ttl));
        self.update_peak();
        if prev.is_none()
            && let Some(k) = new_key_copy
        {
            self.notify_events.push((crate::notify::KeyspaceEvent::New, k));
        }
        prev
    }

    /// Remove a key, returning the displaced entry (`None` if absent).
    /// Frees the entry's cached weight; its slot stays, and stays charged,
    /// with the table. This is the
    /// DISCARD form: a cold stub's vlog record is credited dead. A
    /// caller re-homing the entry intact (RENAME) uses
    /// [`Self::take_entry_keepalive`] instead.
    pub(crate) fn remove_entry(&mut self, key: &[u8]) -> Option<Entry> {
        let old = self.take_entry_keepalive(key)?;
        self.tier_note_dead(key_heap_bytes_for(key), &old.value);
        Some(old)
    }

    /// [`Self::remove_entry`] minus the cold-record dead-credit — for
    /// moves that keep the entry (and any [`crate::value::ColdRef`] in
    /// it) alive under another key. Same accounting/hfttl behaviour.
    pub(crate) fn take_entry_keepalive(&mut self, key: &[u8]) -> Option<Entry> {
        self.clear_hash_key_ttls(key);
        let old = self.map.remove(key)?;
        self.used_memory = self.used_memory.saturating_sub(old.weight());
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
        if let Some(e) = self.map.get_mut_quiet(key) {
            e.add_to_weight(delta);
        }
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Charge what the keyspace table grew by. The table is charged as a
    /// whole, at its real size: a slot costs nothing more when a key takes
    /// it and nothing less when one leaves, and only a growth moves it.
    #[cold]
    pub(crate) fn charge_keyspace_growth(&mut self) {
        let now = self.map.footprint() as u64;
        let delta = now as i64 - self.keyspace_bytes as i64;
        self.keyspace_bytes = now;
        apply_delta(&mut self.used_memory, delta);
    }

    /// Recompute `weight` for the entry at `key` from its current value +
    /// key, then propagate the delta to `used_memory`. Use after a wholesale
    /// in-place value swap (SET / APPEND / INCRBYFLOAT) where the prior
    /// `Value`'s weight was already cached on the entry.
    pub(crate) fn reweigh_entry(&mut self, key: &[u8]) {
        let key_heap = key_heap_bytes_for(key);
        let Some(e) = self.map.get_mut_quiet(key) else {
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
            self.note_expired(key);
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
    /// One keyspace probe on a hit: the slot it finds serves the expiry
    /// check, the access-clock touch and the returned borrow. The clock is
    /// read only when the entry carries a TTL. An expired key takes the cold
    /// path through [`Self::drop_expired`], which probes again.
    pub(crate) fn live_entry(&mut self, key: &[u8]) -> Option<&Entry> {
        let slot = self.map.find_slot(key)?;
        if self.slot_expired(slot) {
            self.drop_expired(key);
            return None;
        }
        if self.clock_on() {
            let (c, policy) = self.tick_touch();
            let e = self.map.entry_at_quiet(slot)?;
            evict::touch_on_access(e, policy, c);
            return Some(&*e);
        }
        self.map.slot(slot).map(|(_, e)| e)
    }

    /// Mutable [`live_entry`](Self::live_entry): the live `Entry` for `key` by
    /// `&mut`, or `None` if absent/expired (expired dropped). Same single
    /// probe; the row recorder sees the row before it is handed out.
    /// Read-modify commands (INCR/APPEND/…) get the entry once and mutate in
    /// place, preserving any TTL on it.
    pub(crate) fn live_entry_mut(&mut self, key: &[u8]) -> Option<&mut Entry> {
        let slot = self.map.find_slot(key)?;
        if self.slot_expired(slot) {
            self.drop_expired(key);
            return None;
        }
        if self.clock_on() {
            let (c, policy) = self.tick_touch();
            let e = self.map.entry_at_mut(key, slot)?;
            evict::touch_on_access(e, policy, c);
            return Some(e);
        }
        self.map.entry_at_mut(key, slot)
    }

    /// Whether the entry at `slot` carries a TTL that has passed.
    #[inline]
    fn slot_expired(&self, slot: usize) -> bool {
        let (uc, cn) = (self.cached_clock, self.cached_ns);
        self.map.slot(slot).is_some_and(|(_, e)| e.expire_at_ns.is_some() && e.is_expired(uc, cn))
    }

    /// Advance the access clock for a live hit; the value and policy to
    /// touch the entry with.
    #[inline]
    fn tick_touch(&mut self) -> (u32, crate::EvictionPolicy) {
        self.tick_clock();
        (self.clock_counter as u32, self.touch_policy())
    }

    /// Drop an expired `key` found by a read, counted as an expiry.
    #[cold]
    fn drop_expired(&mut self, key: &[u8]) {
        self.note_expired(key);
        self.remove_entry(key);
        self.expired_keys_total = self.expired_keys_total.saturating_add(1);
    }
}
