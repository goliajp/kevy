//! Slot-index access: probe once, then reach the same entry again without
//! hashing.
//!
//! A slot index is only a position. Any insert or remove may move, free or
//! reuse it, so every access re-checks that the slot is occupied and hands
//! back the key stored there; a caller that keeps an index across
//! mutations compares that key before trusting the value. Nothing here can
//! read an uninitialised slot.

use core::borrow::Borrow;

use kevy_hash::KevyHash;

use crate::map::KevyMap;

impl<K, V> KevyMap<K, V> {
    /// The slot `key` occupies, or `None` if it is absent. One probe; the
    /// index stays valid until the next insert or remove.
    ///
    /// ```
    /// let mut m = kevy_map::KevyMap::new();
    /// m.insert(7u64, "seven");
    /// let slot = m.find_slot(&7).ok_or("present")?;
    /// assert_eq!(m.slot(slot), Some((&7, &"seven")));
    /// assert_eq!(m.find_slot(&8), None);
    /// # Ok::<(), &str>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn find_slot<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        self.find_by_borrow(key)
    }

    /// The key and value at `slot`, or `None` when the slot is out of range
    /// or not occupied. No hashing; the returned key says whose entry it is.
    ///
    /// ```
    /// let mut m = kevy_map::KevyMap::new();
    /// m.insert(1u64, 10u32);
    /// let slot = m.find_slot(&1).ok_or("present")?;
    /// m.remove(&1);
    /// assert_eq!(m.slot(slot), None, "a freed slot reads as empty");
    /// assert_eq!(m.slot(usize::MAX), None);
    /// # Ok::<(), &str>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn slot(&self, slot: usize) -> Option<(&K, &V)> {
        if !self.slot_is_full(slot) {
            return None;
        }
        // SAFETY: `slot_is_full` checked `slot < cap` and that its metadata
        // byte marks an initialised entry.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
        Some((&kv.0, &kv.1))
    }

    /// Mutable [`slot`](Self::slot): the key and a mutable value at `slot`,
    /// or `None` when the slot is out of range or not occupied.
    ///
    /// ```
    /// let mut m = kevy_map::KevyMap::new();
    /// m.insert(1u64, 10u32);
    /// let slot = m.find_slot(&1).ok_or("present")?;
    /// if let Some((&1, v)) = m.slot_mut(slot) {
    ///     *v += 1;
    /// }
    /// assert_eq!(m.get(&1), Some(&11));
    /// # Ok::<(), &str>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn slot_mut(&mut self, slot: usize) -> Option<(&K, &mut V)> {
        if !self.slot_is_full(slot) {
            return None;
        }
        // SAFETY: as in `slot`.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_mut() };
        Some((&kv.0, &mut kv.1))
    }

    #[inline]
    fn slot_is_full(&self, slot: usize) -> bool {
        // SAFETY: read only after the bound check; the metadata array holds
        // at least `cap` bytes.
        slot < self.cap && unsafe { *self.metadata_ptr.as_ptr().add(slot) } & 0x80 == 0
    }
}

#[cfg(test)]
mod tests {
    use crate::KevyMap;

    #[test]
    fn a_slot_follows_its_entry_until_the_table_changes_shape() {
        let mut m: KevyMap<u64, u64> = KevyMap::new();
        assert_eq!(m.slot(0), None, "an unallocated table has no slots");
        for k in 0..crate::scaled(1000) as u64 {
            m.insert(k, k * 2);
        }
        for k in 0..crate::scaled(1000) as u64 {
            let s = m.find_slot(&k).expect("present");
            assert_eq!(m.slot(s), Some((&k, &(k * 2))));
            *m.slot_mut(s).expect("full").1 += 1;
            assert_eq!(m.get(&k), Some(&(k * 2 + 1)));
        }
        let s = m.find_slot(&5).expect("present");
        m.remove(&5);
        assert_eq!(m.slot(s), None);
        assert!(m.slot_mut(s).is_none());
        let full = (0..m.capacity()).filter(|&i| m.slot(i).is_some()).count();
        assert_eq!(full, m.len(), "exactly the occupied slots read as full");
    }
}
