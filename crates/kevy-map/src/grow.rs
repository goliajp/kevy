//! Growth: a table twice the size, and every entry moved into it.
//!
//! Growth rebuilds rather than rehashes in place, and reinserts through
//! `insert_known_unique`, which skips the key comparison entirely — the old
//! table already proved every key distinct. That turns the rehash into one
//! `match_byte(EMPTY)` per key.

use core::ptr;

use kevy_hash::KevyHash;

use crate::group::Group;
use crate::map::{DELETED, EMPTY, GROUP_WIDTH, KevyMap, MIN_CAP, h2};

/// How much of a mapped table's emptied slot range is handed back at once:
/// its huge page.
const RELEASE_STEP: usize = 2 * 1024 * 1024;

impl<K: KevyHash + Eq, V> KevyMap<K, V> {
    pub(crate) fn grow(&mut self) {
        let new_cap = if self.cap == 0 {
            MIN_CAP
        } else {
            self.cap
                .checked_mul(2)
                .expect("a capacity that overflows usize could not have been allocated")
        };
        let mut new_table = Self::alloc_table(new_cap);
        if self.aux.is_some() {
            new_table.aux = Some(crate::aux::lane(new_cap));
        }
        // Move every live entry over. After ptr::read'ing a slot we mark its
        // metadata DELETED, so any subsequent Drop (incl. panic unwind) won't
        // double-free; the old allocation will free with all-DELETED metadata.
        //
        // Only iterate the real slot range `[0, cap)`; the trailing mirror
        // bytes are bookkeeping for SIMD-load wraparound, not real slots.
        // Direct metadata writes are safe here because the old `self` table
        // is going away (we swap with new_table then drop), so a stale mirror
        // doesn't matter.
        let old_cap = self.cap;
        // A mapped table hands its slot pages back as the move passes them.
        // Entries leave in bucket order and land at `hash & new_mask`, which
        // walks the new table in the same order, so the new table fills as
        // the old one empties and the two are never both whole in memory.
        let slot_bytes = core::mem::size_of::<(K, V)>();
        let mut released = 0usize;
        for i in 0..old_cap {
            if self.mmap_backed && (i * slot_bytes) - released >= RELEASE_STEP {
                released = self.release_moved(released, i * slot_bytes);
            }
            self.move_slot(i, &mut new_table);
        }
        // All occupied entries are now in new_table; the old self has no live slots.
        self.occupied = 0;
        self.deleted = 0;
        core::mem::swap(self, &mut new_table);
        // new_table (now the old self) drops; metadata is all DELETED (or EMPTY
        // for previously-empty slots) ⇒ Drop walks but touches no slots.
    }

    /// Move the entry at slot `i`, if any, and its side word into `to`.
    #[inline]
    fn move_slot(&mut self, i: usize, to: &mut Self) {
        // SAFETY: i < cap ⇒ metadata in-bounds.
        let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
        if meta & 0x80 != 0 {
            return;
        }
        // SAFETY: full slot ⇒ initialised; we mark DELETED immediately
        // so this byte is never re-read as occupied.
        let (k, v) = unsafe { ptr::read(self.slots_ptr.as_ptr().add(i) as *const (K, V)) };
        // SAFETY: `i < cap`, so this is inside the metadata range. Writing DELETED
        // immediately is what keeps the `ptr::read` above from being a double move:
        // the byte is never seen as occupied again.
        unsafe { *self.metadata_ptr.as_ptr().add(i) = DELETED };
        let hash = k.kevy_hash();
        let at = to.insert_known_unique(hash, k, v);
        if let (Some(old), Some(new)) = (&self.aux, &mut to.aux) {
            new[at] = old[i];
        }
    }

    /// Hand back the huge pages of slot bytes `[released, moved)` rounded
    /// down to a whole page, and answer how far that reached.
    #[cold]
    fn release_moved(&self, released: usize, moved: usize) -> usize {
        let upto = moved & !(RELEASE_STEP - 1);
        // SAFETY: the slots below `moved` have been moved out and are never
        // read again, and `[released, upto)` lies inside them: a 2 MiB-aligned
        // range at the start of this table's own aligned mapping.
        unsafe {
            let from = self.slots_ptr.cast::<u8>().add(released);
            kevy_madvise::release_2mb(from, upto - released);
        }
        upto
    }

    /// Insert under the assumption that the key isn't already present (used
    /// by `grow` to repopulate the new table). Skips the duplicate-key
    /// check. Uses a 16-slot SIMD group scan to find the first EMPTY.
    fn insert_known_unique(&mut self, hash: u64, k: K, v: V) -> usize {
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask();
        loop {
            // SAFETY: metadata is `cap + GROUP_WIDTH` bytes; group_start
            // is in `[0, cap)`; the load reads 16 bytes which lie inside the
            // buffer thanks to the mirror tail.
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                let slot = (group_start + m) & self.mask();
                self.set_meta(slot, h2v);
                // SAFETY: slot < cap.
                unsafe {
                    (*self.slots_ptr.as_ptr().add(slot)).write((k, v));
                }
                self.occupied += 1;
                return slot;
            }
            // Linear probing by GROUP_WIDTH (tried triangular — at our 7/8
            // load factor and group-scan-aware probe, linear wins on cache
            // locality; triangular's anti-clustering only pays off at higher
            // load factors than we run).
            group_start = (group_start + GROUP_WIDTH) & self.mask();
        }
    }
}
