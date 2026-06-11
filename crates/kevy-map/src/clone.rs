//! `Clone` for [`KevyMap`] — a faithful layout copy.
//!
//! The clone preserves the source's exact bucket geometry: same capacity,
//! same slot positions, same tombstones. Tombstones must be carried over —
//! probe sequences walk *through* `DELETED` bytes and stop at `EMPTY`, so
//! dropping a tombstone would terminate a probe early and lose every key
//! that was inserted past it. Copying positions verbatim (instead of
//! re-inserting) also needs no `KevyHash` bound: only `K: Clone, V: Clone`.
//!
//! Panic safety: the new table starts all-`EMPTY` and a slot's metadata
//! byte is only flipped to occupied *after* the cloned pair is written, so
//! if a `K`/`V` clone panics mid-way the partially-built map drops exactly
//! the entries it actually owns.

use crate::map::{DELETED, GROUP_WIDTH, KevyMap};

impl<K: Clone, V: Clone> Clone for KevyMap<K, V> {
    fn clone(&self) -> Self {
        if self.cap == 0 {
            return Self::new();
        }
        let mut new = Self::alloc_table(self.cap);
        debug_assert_eq!(new.cap, self.cap);
        let _ = GROUP_WIDTH; // mirror bytes are maintained by set_meta below
        for i in 0..self.cap {
            // SAFETY: i < cap ⇒ metadata pointer in-bounds.
            let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
            if meta & 0x80 == 0 {
                // SAFETY: occupied slot ⇒ initialised; shared borrow of the
                // source pair for the clones.
                let (k, v) = unsafe { &*(self.slots_ptr.as_ptr().add(i) as *const (K, V)) };
                let pair = (k.clone(), v.clone());
                // SAFETY: i < cap ⇒ slot pointer in-bounds; the target slot
                // is uninitialised (metadata still EMPTY).
                unsafe {
                    (*new.slots_ptr.as_ptr().add(i)).write(pair);
                }
                new.set_meta(i, meta); // after the write — panic-safe order
            } else if meta == DELETED {
                new.set_meta(i, DELETED);
            }
        }
        new.occupied = self.occupied;
        new.deleted = self.deleted;
        new
    }
}
