//! What a heap-backed hash holds, and what each write adds to it.
//!
//! A hash is charged what the allocator holds for it, counted the way
//! glibc's `malloc` hands out blocks ([`kevy_map::malloc_footprint`]): the
//! `Arc` box around the map, the map's table, and the heap of every field
//! name and value too long to sit in its slot. The table is charged as it
//! grows, not per field — a field fills a slot the table already has, and
//! only a growth step changes what is held.

use kevy_map::{RawEntryMut, malloc_footprint};

use crate::hash::HashRefMut;
use crate::seg_map::arc_box;
use crate::value::{HashData, SmallBytes};

/// Heap bytes `b` holds outside its slot, as the allocator holds them.
#[inline]
pub(crate) fn held(b: &SmallBytes) -> u64 {
    malloc_footprint(b.heap_bytes()) as u64
}

/// [`crate::Value::weight`]'s Hash arm.
pub(crate) fn flat_hash_weight(h: &HashData) -> u64 {
    arc_box::<HashData>()
        + h.footprint() as u64
        + h.iter().map(|(f, v)| held(f) + held(v)).sum::<u64>()
}

impl HashRefMut<'_> {
    /// Insert, and by how much the hash's weight moved: a new field's
    /// heap, or an overwritten value's change, plus any table growth.
    // left to itself the compiler outlines this and the map's wrapper
    // below it, two calls deep with the result returned through memory:
    // measurable on every hash write on an in-order core
    #[allow(clippy::inline_always)]
    #[inline(always)]
    pub(crate) fn insert_weighed(
        &mut self,
        field: SmallBytes,
        value: SmallBytes,
    ) -> (Option<SmallBytes>, i64) {
        let (field_heap, value_heap) = (field.heap_bytes(), value.heap_bytes());
        let (old, grown) = match self {
            Self::Flat(h) => {
                let (old, grown) = h.insert_sized(field, value);
                (old, grown as i64)
            }
            Self::Seg(h) => h.insert_sized(field, value),
        };
        let delta = match &old {
            None => (malloc_footprint(field_heap) + malloc_footprint(value_heap)) as i64,
            Some(o) => malloc_footprint(value_heap) as i64 - held(o) as i64,
        };
        (old, delta + grown)
    }

    /// Remove `field`; `Some(weight it took with it)` when it was there.
    /// The table does not shrink, so the pair's own heap is all that goes.
    #[inline]
    pub(crate) fn remove_weighed(&mut self, field: &[u8]) -> Option<u64> {
        let (key_heap, value) = match self {
            Self::Flat(h) => match h.raw_entry_mut(field) {
                RawEntryMut::Occupied(e) => (e.key().heap_bytes(), e.remove()),
                RawEntryMut::Vacant(_) => return None,
            },
            Self::Seg(h) => h.remove_keyed(field)?,
        };
        Some(malloc_footprint(key_heap) as u64 + held(&value))
    }
}
