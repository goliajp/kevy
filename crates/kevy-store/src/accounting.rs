//! Internal accounting helpers on [`Store`]: per-entry weight bookkeeping,
//! LRU/LFU clock advance, prefetch, and the lazy-expire `live_entry` /
//! `live_entry_mut` lookups used by every typed accessor.
//!
//! Split out of [`crate`] for file-size hygiene. Nothing here is part of
//! the public surface — all methods are `pub(crate)` and called by sibling
//! modules (string/hash/list/set/zset/evict/expire/keyspace).

use kevy_hash::KevyHash;

use crate::entry_weight::{kept, kept_half, set_clock, shift, stamp, value_weight};
use crate::{Entry, SmallBytes, Store, apply_delta, evict, key_heap_bytes_for};

impl Store {
    /// Insert a fresh entry, replacing any prior. Records the value's weight
    /// (and, under an eviction policy, its clock) in the slot's side word,
    /// then updates `used_memory` by the weight swap and by whatever the
    /// keyspace table grew to make room.
    pub(crate) fn insert_entry(&mut self, key: SmallBytes, entry: Entry) -> Option<Entry> {
        self.insert_entry_at(key, entry).1
    }

    /// [`Self::insert_entry`], also returning the slot the entry went to:
    /// valid until the table next inserts or removes.
    pub(crate) fn insert_entry_at(
        &mut self,
        key: SmallBytes,
        entry: Entry,
    ) -> (usize, Option<Entry>) {
        // New-key event capture: the owned key copy is only paid when
        // the capture flag is on (server with `n` notifications).
        let new_key_copy =
            (self.notify_capture & crate::notify::CAPTURE_NEW != 0).then(|| key.to_vec());
        // A wholesale value replacement (type change / RESTORE)
        // discards any per-field hash TTLs; a fresh create is a no-op.
        self.clear_hash_key_ttls(key.as_slice());
        let key_heap = key.heap_bytes() as u64;
        let vw = entry.value.weight();
        let is_kept = kept(&entry.value);
        let clock = self.clock_on().then(|| {
            self.tick_clock();
            self.clock_counter as u32
        });
        if is_kept || clock.is_some() {
            self.keep_words();
        }
        let new_w = key_heap + vw;
        let new_has_ttl = entry.expire_at_ns.is_some();
        let cap = self.map.capacity();
        let (slot, prev) = self.map.insert_slot(key, entry);
        if self.map.capacity() != cap {
            self.charge_keyspace_growth();
        }
        // an overwrite keeps the slot's word, which still holds the
        // displaced value's weight
        let old_w =
            prev.as_ref().map(|old| key_heap + value_weight(&old.value, self.map.aux(slot)));
        self.stamp_slot(slot, is_kept, vw, clock);
        match (&prev, old_w) {
            (Some(old), Some(w)) => {
                // A displaced cold stub's vlog record dies with it.
                self.tier_note_dead(key_heap, &old.value);
                self.used_memory = self.used_memory.saturating_sub(w).saturating_add(new_w);
            }
            _ => self.used_memory = self.used_memory.saturating_add(new_w),
        }
        let old_has_ttl = prev.as_ref().is_some_and(|o| o.expire_at_ns.is_some());
        self.adjust_expires(i64::from(new_has_ttl) - i64::from(old_has_ttl));
        self.update_peak();
        self.note_new_key(prev.is_none(), new_key_copy);
        (slot, prev)
    }

    /// Queue the new-key event for a key just created, when one is wanted.
    fn note_new_key(&mut self, created: bool, copy: Option<alloc::vec::Vec<u8>>) {
        if created && let Some(k) = copy {
            self.notify_events.push((crate::notify::KeyspaceEvent::New, k));
        }
    }

    /// Record a value's weight, and the access clock when one runs, in the
    /// side word of the entry at `slot`.
    #[inline]
    fn stamp_slot(&mut self, slot: usize, is_kept: bool, weight: u64, clock: Option<u32>) {
        if let Some(word) = self.map.word_mut(slot) {
            stamp(word, is_kept, weight);
            if let Some(c) = clock {
                set_clock(word, c);
            }
        }
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
        let (old, word) = self.map.remove_with_word(key)?;
        let old_w = key_heap_bytes_for(key) + value_weight(&old.value, word);
        self.used_memory = self.used_memory.saturating_sub(old_w);
        if old.expire_at_ns.is_some() {
            self.adjust_expires(-1);
        }
        Some(old)
    }

    /// Apply a signed weight delta to `key`'s kept weight AND to the
    /// shard-wide `used_memory`. Used by in-place collection mutators (HSET
    /// adding a field, LPUSH adding an item, …) so we account in O(1)
    /// without re-walking the container. Only a value whose weight is kept
    /// moves by a delta: a small inline one weighs nothing either side of a
    /// change, and one that changes form is reweighed from scratch.
    pub(crate) fn account_delta(&mut self, key: &[u8], delta: i64) {
        if delta == 0 {
            return;
        }
        let slot = self.map.find_slot(key);
        self.account_delta_at(slot, delta);
    }

    /// [`Self::account_delta`] for the entry at `slot`, already found.
    pub(crate) fn account_delta_at(&mut self, slot: Option<usize>, delta: i64) {
        if delta == 0 {
            return;
        }
        if let Some(slot) = slot {
            debug_assert!(self.map.slot(slot).is_some_and(|(_, e)| kept(&e.value)));
            self.keep_words();
            if let Some(word) = self.map.word_mut(slot) {
                shift(word, delta);
            }
        }
        apply_delta(&mut self.used_memory, delta);
        if delta > 0 {
            self.update_peak();
        }
    }

    /// Keep side words from now on; the lane, allocated once, is charged
    /// with the table it belongs to.
    #[inline]
    pub(crate) fn keep_words(&mut self) {
        if self.map.keep_words() {
            self.charge_keyspace_growth();
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

    /// Recompute the weight of the collection at `key` after an in-place
    /// change, then propagate the delta to `used_memory`. The weight it had
    /// is the kept half of its side word (zero for a small inline one).
    pub(crate) fn reweigh_entry(&mut self, key: &[u8]) {
        self.reweigh(key, None);
    }

    /// [`Self::reweigh_entry`] for a value whose weight is worked out when
    /// asked (a string rewritten in place): the caller measured `old` before
    /// the rewrite, since nothing kept it.
    pub(crate) fn reweigh_scalar(&mut self, key: &[u8], old: u64) {
        self.reweigh(key, Some(old));
    }

    fn reweigh(&mut self, key: &[u8], old: Option<u64>) {
        if let Some(slot) = self.map.find_slot(key) {
            self.reweigh_at(slot, old);
        }
    }

    /// [`Self::reweigh_entry`] for the entry at `slot`, already found.
    pub(crate) fn reweigh_at(&mut self, slot: usize, old: Option<u64>) {
        let is_kept = self.map.slot(slot).is_some_and(|(_, e)| kept(&e.value));
        if is_kept {
            self.keep_words();
        }
        let Some((e, word)) = self.map.entry_word_quiet(slot) else {
            return;
        };
        let vw = e.value.weight();
        let was = old.unwrap_or_else(|| kept_half(word.as_deref().copied()));
        if let Some(word) = word {
            stamp(word, is_kept, vw);
        }
        let delta = vw as i64 - was as i64;
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
        self.prefetch_for_hash(key.kevy_hash());
    }

    /// [`Store::prefetch_for_key`] for a key whose hash (`kevy_hash`) the
    /// caller already has.
    #[inline]
    pub fn prefetch_for_hash(&self, hash: u64) {
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
            self.touch_slot(slot);
        }
        self.map.slot(slot).map(|(_, e)| e)
    }

    /// Mutable [`live_entry`](Self::live_entry): the live `Entry` for `key` by
    /// `&mut`, or `None` if absent/expired (expired dropped). Same single
    /// probe; the row recorder sees the row before it is handed out.
    /// Read-modify commands (INCR/APPEND/…) get the entry once and mutate in
    /// place, preserving any TTL on it.
    pub(crate) fn live_entry_mut(&mut self, key: &[u8]) -> Option<&mut Entry> {
        let slot = self.live_slot(key)?;
        self.map.entry_at_mut(key, slot)
    }

    /// The slot of `key`'s live entry — an expired one is dropped — with
    /// its access clock touched: one probe for a command that then works
    /// on the entry through the slot.
    pub(crate) fn live_slot(&mut self, key: &[u8]) -> Option<usize> {
        let slot = self.map.find_slot(key)?;
        if self.slot_expired(slot) {
            self.drop_expired(key);
            return None;
        }
        if self.clock_on() {
            self.touch_slot(slot);
        }
        Some(slot)
    }

    /// [`Self::live_entry_mut`] with the entry's side word, for a write that
    /// replaces the value and so must know what the old one weighed.
    pub(crate) fn live_entry_word_mut(
        &mut self,
        key: &[u8],
    ) -> Option<(&mut Entry, Option<&mut u64>)> {
        let slot = self.map.find_slot(key)?;
        if self.slot_expired(slot) {
            self.drop_expired(key);
            return None;
        }
        if self.clock_on() {
            self.touch_slot(slot);
        }
        self.map.entry_word_at_mut(key, slot)
    }

    /// Whether the entry at `slot` carries a TTL that has passed.
    #[inline]
    fn slot_expired(&self, slot: usize) -> bool {
        let (uc, cn) = (self.cached_clock, self.cached_ns);
        self.map.slot(slot).is_some_and(|(_, e)| e.expire_at_ns.is_some() && e.is_expired(uc, cn))
    }

    /// Stamp the access clock on a live hit's side word.
    #[inline]
    fn touch_slot(&mut self, slot: usize) {
        let (c, policy) = self.tick_touch();
        self.keep_words();
        if let Some(word) = self.map.word_mut(slot) {
            evict::touch_on_access(word, policy, c);
        }
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
