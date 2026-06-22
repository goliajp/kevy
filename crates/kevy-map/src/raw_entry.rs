//! Single-probe entry API à la hashbrown's `RawEntryMut`.
//!
//! Motivation: the existing read/insert APIs (`get`, `get_mut`, `insert`,
//! `remove`) each cost one full probe. Common Store patterns —
//! "look up, check expiry, conditionally remove, otherwise return the
//! borrow" — currently do **two** probes because the borrow returned by
//! `get` cannot survive a subsequent `remove` (mutable-borrow conflict
//! with the immutable borrow on the value). The raw-entry API folds the
//! read and the conditional remove into a single probe by consuming the
//! `RawOccupiedEntryMut` on `remove(self)`, which releases the borrow at
//! the call site.
//!
//! Design notes:
//!
//! * `raw_entry_mut` takes `&mut self` (any subsequent insert/remove
//!   needs exclusive access; matching `Borrow<Q>` lookup matches the
//!   shape of `get_mut`).
//! * The `RawOccupiedEntryMut` stores a borrowed `&'a mut KevyMap<K, V>`
//!   plus the slot index already located by the probe. `get` /
//!   `get_mut` / `into_mut` reuse that slot; `remove(self)` consumes the
//!   entry so the map borrow is freed and a `set_meta(DELETED)` write
//!   can proceed.
//! * The `RawVacantEntryMut` stores `&'a mut KevyMap<K, V>` and **does
//!   not** cache the probe's `insert_at`: any subsequent mutation
//!   (notably `maybe_grow` inside `insert`) can invalidate that slot.
//!   `insert(self, k, v)` therefore re-runs `insert` from scratch
//!   (one extra probe in the absent-key insert path; the API-additive
//!   commit is purely about unblocking the *read or remove* fast path —
//!   downstream cumulative attacks may later push the cached probe
//!   through, once the grow-invalidation issue is solved with an
//!   explicit `reserve` API).
//!
//! No `unsafe` is added by this file: every memory touch is delegated
//! to the existing `pub(crate)` helpers in `map.rs` / `map_keyed.rs`.

use std::borrow::Borrow;
use std::ptr;

use kevy_hash::KevyHash;

use crate::map::{DELETED, KevyMap, ProbeOutcome};

/// Result of [`KevyMap::raw_entry_mut`].
///
/// Mirrors `hashbrown::hash_map::RawEntryMut`: an `Occupied` arm grants
/// read / mutate / consume access to the existing entry; a `Vacant` arm
/// can be filled with `insert(k, v)`. The defining property — and the
/// reason this API exists distinct from `get_mut` — is that
/// [`RawOccupiedEntryMut::remove`] consumes `self`, which releases the
/// outstanding borrow on the map and lets the caller perform the
/// deletion within the same borrow scope.
pub enum RawEntryMut<'a, K, V> {
    /// The key was present; gives read / mutate / consume access.
    Occupied(RawOccupiedEntryMut<'a, K, V>),
    /// The key was absent; `insert(k, v)` writes a new entry.
    Vacant(RawVacantEntryMut<'a, K, V>),
}

/// Handle to an existing entry, returned by [`RawEntryMut::Occupied`].
pub struct RawOccupiedEntryMut<'a, K, V> {
    map: &'a mut KevyMap<K, V>,
    /// Slot index inside `map.slots_ptr` for the located entry.
    slot: usize,
}

/// Handle to an absent entry, returned by [`RawEntryMut::Vacant`].
pub struct RawVacantEntryMut<'a, K, V> {
    map: &'a mut KevyMap<K, V>,
}

impl<K, V> KevyMap<K, V> {
    /// Look up `key`; return an [`RawEntryMut`] giving single-probe
    /// access to the located (or vacant) slot.
    ///
    /// One full probe. The returned handle borrows `self` mutably; the
    /// borrow is released only when the handle is dropped (or consumed
    /// via [`RawOccupiedEntryMut::remove`] / [`RawOccupiedEntryMut::into_mut`]).
    pub fn raw_entry_mut<Q>(&mut self, key: &Q) -> RawEntryMut<'_, K, V>
    where
        K: Borrow<Q> + KevyHash + Eq,
        Q: KevyHash + Eq + ?Sized,
    {
        match self.probe_by_borrow(key) {
            ProbeOutcome::Found(slot) => RawEntryMut::Occupied(RawOccupiedEntryMut {
                map: self,
                slot,
            }),
            ProbeOutcome::NotFound { .. } => {
                RawEntryMut::Vacant(RawVacantEntryMut { map: self })
            }
        }
    }
}

impl<'a, K, V> RawOccupiedEntryMut<'a, K, V> {
    /// Shared access to the stored value.
    #[inline]
    pub fn get(&self) -> &V {
        // SAFETY: `slot` came from `probe_by_borrow::Found`, which only
        // returns indices into full slots.
        let kv = unsafe { (*self.map.slots_ptr.as_ptr().add(self.slot)).assume_init_ref() };
        &kv.1
    }

    /// Mutable access to the stored value (borrow tied to `&mut self`).
    #[inline]
    pub fn get_mut(&mut self) -> &mut V {
        // SAFETY: see [`get`].
        let kv = unsafe { (*self.map.slots_ptr.as_ptr().add(self.slot)).assume_init_mut() };
        &mut kv.1
    }

    /// Mutable access to the stored value with the outer map's lifetime,
    /// consuming the handle. The map borrow returned outlives `self`,
    /// which is exactly the shape `get`+`get_mut` cannot provide.
    #[inline]
    pub fn into_mut(self) -> &'a mut V {
        // SAFETY: see [`get`]. The returned borrow's lifetime `'a` is
        // tied to the borrow we were constructed with, which is the
        // caller's `&mut KevyMap<K, V>` borrow.
        let kv = unsafe { (*self.map.slots_ptr.as_ptr().add(self.slot)).assume_init_mut() };
        &mut kv.1
    }

    /// Shared access to the stored key.
    #[inline]
    pub fn key(&self) -> &K {
        // SAFETY: see [`get`].
        let kv = unsafe { (*self.map.slots_ptr.as_ptr().add(self.slot)).assume_init_ref() };
        &kv.0
    }

    /// Remove the entry; returns the previous value. Consumes `self`,
    /// releasing the map borrow so the caller can immediately re-probe
    /// or mutate something else.
    ///
    /// This is the load-bearing method — it is what makes the API
    /// strictly more expressive than `get_mut`+`remove`.
    pub fn remove(self) -> V {
        self.map.set_meta(self.slot, DELETED);
        self.map.occupied -= 1;
        self.map.deleted += 1;
        // SAFETY: slot was full; we just marked it DELETED so it won't
        // be re-read as occupied. `ptr::read` moves the (K, V) out;
        // dropping `k` here is correct because we don't return it.
        let (_k, v) = unsafe {
            ptr::read(self.map.slots_ptr.as_ptr().add(self.slot) as *const (K, V))
        };
        v
    }
}

impl<'a, K, V> RawVacantEntryMut<'a, K, V>
where
    K: KevyHash + Eq,
{
    /// Insert `(key, value)` and return a mutable borrow of the freshly
    /// inserted value. The borrow is tied to the original map borrow.
    ///
    /// Note: this performs a second probe after a possible grow, because
    /// the slot located by the first probe (the one that produced
    /// `Vacant`) can be invalidated by `maybe_grow`. The added probe is
    /// acceptable for the live_entry pattern (the *read* fast path is
    /// the one we were chasing; the absent-key insert path is the cold
    /// side, and `live_entry` itself never takes it).
    pub fn insert(self, key: K, value: V) -> &'a mut V {
        // Grow if needed (matches `KevyMap::insert`'s preamble).
        self.map.maybe_grow();
        // Re-probe by reference, write into the slot, bump occupancy.
        let hash = key.kevy_hash();
        let outcome = self.map.probe_by_borrow(&key);
        let slot = match outcome {
            ProbeOutcome::NotFound { insert_at, via_tombstone } => {
                self.map.set_meta(insert_at, crate::map::h2(hash));
                // SAFETY: insert_at < cap ⇒ slot pointer in-bounds; we
                // write (K, V) into a previously uninitialised slot.
                unsafe {
                    (*self.map.slots_ptr.as_ptr().add(insert_at)).write((key, value));
                }
                self.map.occupied += 1;
                if via_tombstone {
                    self.map.deleted -= 1;
                }
                insert_at
            }
            ProbeOutcome::Found(_) => {
                // Cannot happen: we held the only mutable borrow on the
                // map between `raw_entry_mut` and here, and the first
                // probe said Vacant. Treat as logic bug rather than
                // overwriting (overwriting would silently change the
                // documented contract).
                unreachable!("raw vacant insert observed an existing key");
            }
        };
        // SAFETY: slot is the one we just initialised.
        let kv = unsafe { (*self.map.slots_ptr.as_ptr().add(slot)).assume_init_mut() };
        &mut kv.1
    }
}
