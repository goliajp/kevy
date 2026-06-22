//! Key-trait-bound `KevyMap` operations: insert/grow/lookup/remove.
//!
//! Split out of [`crate::map`] for file-size hygiene. The raw / non-keyed
//! impl block (allocation, metadata bookkeeping, iter, Drop, trait impls)
//! stays in `map.rs`; everything that needs `K: KevyHash + Eq` or
//! `K: Borrow<Q>, Q: KevyHash + Eq` lives here.

use std::borrow::Borrow;
use std::ptr;

use kevy_hash::KevyHash;

use crate::group::Group;
use crate::map::{DELETED, EMPTY, GROUP_WIDTH, KevyMap, MIN_CAP, ProbeOutcome, h2};

impl<K: KevyHash + Eq, V> KevyMap<K, V> {
    /// Insert `(key, value)`. Returns the old value if `key` was already
    /// present. Following `std::HashMap` semantics, the existing K is kept on
    /// overwrite — only V is replaced.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.maybe_grow();
        let hash = key.kevy_hash();
        match self.probe_with_key(hash, &key) {
            ProbeOutcome::Found(idx) => {
                // SAFETY: slot is full ⇒ initialised. We replace only the V
                // field; the old K is kept (std HashMap semantics).
                let v_ptr = unsafe {
                    let kv: *mut (K, V) = self.slots_ptr.as_ptr().add(idx).cast::<(K, V)>();
                    ptr::addr_of_mut!((*kv).1)
                };
                let old_v = unsafe { ptr::replace(v_ptr, value) };
                drop(key);
                Some(old_v)
            }
            ProbeOutcome::NotFound {
                insert_at,
                via_tombstone,
            } => {
                self.set_meta(insert_at, h2(hash));
                // SAFETY: insert_at < cap ⇒ slot pointer in-bounds; we write
                // (K, V) into a previously uninitialised slot.
                unsafe {
                    (*self.slots_ptr.as_ptr().add(insert_at)).write((key, value));
                }
                self.occupied += 1;
                if via_tombstone {
                    self.deleted -= 1;
                }
                None
            }
        }
    }

    pub(crate) fn maybe_grow(&mut self) {
        if self.cap == 0 || (self.occupied + self.deleted) >= self.threshold() {
            self.grow();
        }
    }

    fn grow(&mut self) {
        let new_cap = if self.cap == 0 {
            MIN_CAP
        } else {
            self.cap
                .checked_mul(2)
                .expect("kevy-map: capacity doubling overflow")
        };
        let mut new_map = Self::alloc_table(new_cap);
        // Move every live entry over. After ptr::read'ing a slot we mark its
        // metadata DELETED, so any subsequent Drop (incl. panic unwind) won't
        // double-free; the old allocation will free with all-DELETED metadata.
        //
        // Only iterate the real slot range `[0, cap)`; the trailing mirror
        // bytes are bookkeeping for SIMD-load wraparound, not real slots.
        // Direct metadata writes are safe here because the old `self` table
        // is going away (we swap with new_map then drop), so a stale mirror
        // doesn't matter.
        let old_cap = self.cap;
        for i in 0..old_cap {
            // SAFETY: i < old_cap ⇒ metadata in-bounds.
            let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
            if meta & 0x80 == 0 {
                // SAFETY: full slot ⇒ initialised; we mark DELETED immediately
                // so this byte is never re-read as occupied.
                let (k, v) = unsafe { ptr::read(self.slots_ptr.as_ptr().add(i) as *const (K, V)) };
                unsafe { *self.metadata_ptr.as_ptr().add(i) = DELETED };
                let hash = k.kevy_hash();
                new_map.insert_known_unique(hash, k, v);
            }
        }
        // All occupied entries are now in new_map; the old self has no live slots.
        self.occupied = 0;
        self.deleted = 0;
        std::mem::swap(self, &mut new_map);
        // new_map (now the old self) drops; metadata is all DELETED (or EMPTY
        // for previously-empty slots) ⇒ Drop walks but touches no slots.
    }

    /// Insert under the assumption that the key isn't already present (used
    /// by `grow` to repopulate the new table). Skips the duplicate-key
    /// check. Uses a 16-slot SIMD group scan to find the first EMPTY.
    fn insert_known_unique(&mut self, hash: u64, k: K, v: V) {
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask;
        loop {
            // SAFETY: metadata is `cap + GROUP_WIDTH` bytes; group_start
            // is in `[0, cap)`; the load reads 16 bytes which lie inside the
            // buffer thanks to the mirror tail.
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                let slot = (group_start + m) & self.mask;
                self.set_meta(slot, h2v);
                // SAFETY: slot < cap.
                unsafe {
                    (*self.slots_ptr.as_ptr().add(slot)).write((k, v));
                }
                self.occupied += 1;
                return;
            }
            // Linear probing by GROUP_WIDTH (tried triangular — at our 7/8
            // load factor and group-scan-aware probe, linear wins on cache
            // locality; triangular's anti-clustering only pays off at higher
            // load factors than we run).
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }

    fn probe_with_key(&self, hash: u64, key: &K) -> ProbeOutcome {
        if self.cap == 0 {
            return ProbeOutcome::NotFound {
                insert_at: 0,
                via_tombstone: false,
            };
        }
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask;

        // Fast path: no tombstones in the table ⇒ skip DELETED tracking
        // entirely. This trims one SIMD `match_byte` (and one branch) from
        // every group iteration; insert workloads with no deletions hit
        // this path exclusively.
        if self.deleted == 0 {
            loop {
                // SAFETY: see [insert_known_unique].
                let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
                for m in g.match_byte(h2v).iter() {
                    let slot = (group_start + m) & self.mask;
                    // SAFETY: matched h2 ⇒ slot is occupied ⇒ initialised.
                    let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                    if &kv.0 == key {
                        return ProbeOutcome::Found(slot);
                    }
                }
                if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                    return ProbeOutcome::NotFound {
                        insert_at: (group_start + m) & self.mask,
                        via_tombstone: false,
                    };
                }
                group_start = (group_start + GROUP_WIDTH) & self.mask;
            }
        }

        // Slow path: tombstones exist; track the first DELETED so insert
        // can reclaim it instead of growing the tombstone count.
        let mut first_deleted: Option<usize> = None;
        loop {
            // SAFETY: see [insert_known_unique].
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            for m in g.match_byte(h2v).iter() {
                let slot = (group_start + m) & self.mask;
                // SAFETY: matched h2 ⇒ slot is occupied ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                if &kv.0 == key {
                    return ProbeOutcome::Found(slot);
                }
            }
            if first_deleted.is_none()
                && let Some(m) = g.match_byte(DELETED).lowest_set()
            {
                first_deleted = Some((group_start + m) & self.mask);
            }
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                let probe_empty = (group_start + m) & self.mask;
                return ProbeOutcome::NotFound {
                    insert_at: first_deleted.unwrap_or(probe_empty),
                    via_tombstone: first_deleted.is_some(),
                };
            }
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }
}

impl<K, V> KevyMap<K, V> {
    /// Borrow the value for `key`, or `None` if absent.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        // SAFETY: find_by_borrow only returns indices into full slots.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(idx)).assume_init_ref() };
        Some(&kv.1)
    }

    /// Mutably borrow the value for `key`, or `None` if absent.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        // SAFETY: full slot.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(idx)).assume_init_mut() };
        Some(&mut kv.1)
    }

    /// Whether `key` is present in the map.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        self.find_by_borrow(key).is_some()
    }

    /// Remove `key`'s entry; returns the previous value if present.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        self.set_meta(idx, DELETED);
        self.occupied -= 1;
        self.deleted += 1;
        // SAFETY: slot was full, we just marked it DELETED so it won't be
        // read again; ptr::read moves the (K, V) out.
        let (_k, v) = unsafe { ptr::read(self.slots_ptr.as_ptr().add(idx) as *const (K, V)) };
        Some(v)
    }

    pub(crate) fn find_by_borrow<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        if self.cap == 0 {
            return None;
        }
        let hash = key.kevy_hash();
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask;
        loop {
            // SAFETY: see [insert_known_unique]; group_start ∈ [0, cap),
            // metadata length ≥ cap + GROUP_WIDTH.
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            for m in g.match_byte(h2v).iter() {
                let slot = (group_start + m) & self.mask;
                // SAFETY: matched h2 ⇒ slot occupied ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                if kv.0.borrow() == key {
                    return Some(slot);
                }
            }
            // EMPTY in this group ⇒ key cannot be later in the probe.
            if !g.match_byte(EMPTY).is_empty() {
                return None;
            }
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }

    /// Full probe: returns `Found(idx)` if `key` is present, else
    /// `NotFound { insert_at, via_tombstone }` describing the slot a future
    /// insert would take. Mirrors [`probe_with_key`](Self::probe_with_key)
    /// but accepts a `Borrow<Q>` key.
    ///
    /// Used by the [`raw_entry_mut`](Self::raw_entry_mut) API to fuse a read
    /// and a possible insert into a single probe.
    pub(crate) fn probe_by_borrow<Q>(&self, key: &Q) -> ProbeOutcome
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        if self.cap == 0 {
            return ProbeOutcome::NotFound {
                insert_at: 0,
                via_tombstone: false,
            };
        }
        let hash = key.kevy_hash();
        let h2v = h2(hash);
        let group_start = (hash as usize) & self.mask;
        if self.deleted == 0 {
            self.probe_by_borrow_fast(key, h2v, group_start)
        } else {
            self.probe_by_borrow_slow(key, h2v, group_start)
        }
    }

    /// Fast path for `probe_by_borrow`: no tombstones in the table, so we
    /// can stop tracking DELETED slots entirely.
    fn probe_by_borrow_fast<Q>(&self, key: &Q, h2v: u8, mut group_start: usize) -> ProbeOutcome
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        loop {
            // SAFETY: see [insert_known_unique].
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            for m in g.match_byte(h2v).iter() {
                let slot = (group_start + m) & self.mask;
                // SAFETY: matched h2 ⇒ slot occupied ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                if kv.0.borrow() == key {
                    return ProbeOutcome::Found(slot);
                }
            }
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                return ProbeOutcome::NotFound {
                    insert_at: (group_start + m) & self.mask,
                    via_tombstone: false,
                };
            }
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }

    /// Slow path for `probe_by_borrow`: tombstones present; remember the
    /// first DELETED so a later insert can reclaim it.
    fn probe_by_borrow_slow<Q>(&self, key: &Q, h2v: u8, mut group_start: usize) -> ProbeOutcome
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let mut first_deleted: Option<usize> = None;
        loop {
            // SAFETY: see [insert_known_unique].
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            for m in g.match_byte(h2v).iter() {
                let slot = (group_start + m) & self.mask;
                // SAFETY: matched h2 ⇒ slot occupied ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                if kv.0.borrow() == key {
                    return ProbeOutcome::Found(slot);
                }
            }
            if first_deleted.is_none()
                && let Some(m) = g.match_byte(DELETED).lowest_set()
            {
                first_deleted = Some((group_start + m) & self.mask);
            }
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                let probe_empty = (group_start + m) & self.mask;
                return ProbeOutcome::NotFound {
                    insert_at: first_deleted.unwrap_or(probe_empty),
                    via_tombstone: first_deleted.is_some(),
                };
            }
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }
}
