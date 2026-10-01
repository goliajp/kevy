//! A word per slot beside the table, for the owner's own bookkeeping.
//!
//! What a map's owner keeps about an entry but rarely reads — a cached
//! size, an access clock — would widen every slot if it lived in the value,
//! and a wider slot spans more cache lines on every lookup. The side lane
//! holds one `u64` per slot instead, allocated only once the owner asks for
//! it, and touched only by the owner's own calls: a lookup never reads it.
//!
//! A slot's word starts at zero when an entry takes the slot, and moves
//! with the entry when the table grows.

use alloc_crate::boxed::Box;
use alloc_crate::vec;

use crate::map::KevyMap;

/// A zeroed lane for `cap` slots.
pub(crate) fn lane(cap: usize) -> Box<Box<[u64]>> {
    Box::new(vec![0u64; cap].into_boxed_slice())
}

impl<K, V> KevyMap<K, V> {
    /// `cap - 1`, the probe's wraparound mask (`cap` is zero or a power of
    /// two; every probe returns early on an empty table).
    #[inline(always)]
    pub(crate) fn mask(&self) -> usize {
        self.cap.wrapping_sub(1)
    }

    /// Keep a side word per slot from now on; a no-op when already kept.
    /// Whether this call allocated the lane (so [`Self::footprint`] grew).
    ///
    /// ```
    /// let mut m: kevy_map::KevyMap<u64, &str> = kevy_map::KevyMap::new();
    /// m.insert(7, "seven");
    /// m.enable_aux();
    /// let slot = m.find_slot(&7).unwrap();
    /// assert_eq!(m.aux(slot), Some(0));
    /// *m.aux_mut(slot).unwrap() = 42;
    /// for i in 0..100 {
    ///     m.insert(100 + i, "grown");
    /// }
    /// // the word moved with its entry when the table grew
    /// assert_eq!(m.aux(m.find_slot(&7).unwrap()), Some(42));
    /// ```
    pub fn enable_aux(&mut self) -> bool {
        if self.aux.is_some() {
            return false;
        }
        self.aux = Some(lane(self.cap));
        true
    }

    /// The side word of the entry at `slot`; `None` when the slot is empty
    /// or no side words are kept.
    #[inline]
    pub fn aux(&self, slot: usize) -> Option<u64> {
        let lane = self.aux.as_ref()?;
        self.slot_is_full(slot).then(|| lane[slot])
    }

    /// The side word of the entry at `slot`, to change; `None` when the
    /// slot is empty or no side words are kept.
    #[inline]
    pub fn aux_mut(&mut self, slot: usize) -> Option<&mut u64> {
        // the lane first: most maps keep none, and then the slot is not read
        let full = self.aux.is_some() && self.slot_is_full(slot);
        self.aux.as_mut().filter(|_| full).map(|lane| &mut lane[slot])
    }

    /// The entry at `slot` and its side word, to change: the value from the
    /// slot array, the word from the side lane (`None` when none is kept).
    #[inline]
    pub fn slot_mut_aux(&mut self, slot: usize) -> Option<(&K, &mut V, Option<&mut u64>)> {
        if !self.slot_is_full(slot) {
            return None;
        }
        // SAFETY: a full slot holds an initialised pair; the word lives in a
        // separate allocation, so the two borrows are disjoint.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_mut() };
        Some((&kv.0, &mut kv.1, self.aux.as_mut().map(|lane| &mut lane[slot])))
    }

    /// An entry has just taken `slot`: its side word starts at zero.
    #[inline]
    pub(crate) fn reset_aux(&mut self, slot: usize) {
        if let Some(lane) = &mut self.aux {
            lane[slot] = 0;
        }
    }

    /// The side lane's bytes, as the allocator holds them: the words, and
    /// the box holding the pointer to them.
    pub(crate) fn aux_footprint(&self) -> usize {
        self.aux.as_ref().map_or(0, |lane| lane_footprint(lane.len()))
    }
}

/// What a lane of `cap` words holds at the allocator.
pub(crate) fn lane_footprint(cap: usize) -> usize {
    let words = if cap == 0 { 0 } else { crate::malloc_footprint(cap * 8) };
    crate::malloc_footprint(core::mem::size_of::<Box<[u64]>>()) + words
}

// A map is 56 bytes, and every hash, set and sorted set holds one: a field
// added here is paid once per collection.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::size_of::<KevyMap<u64, u64>>() == 56);
