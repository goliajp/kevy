//! What a sharded hash holds at the allocator, and how a write moves it.
//!
//! The structure around the entries is the directory, the bucket pointer
//! array, and every bucket's `Arc` box and table. A write touches one
//! bucket, so it is charged that bucket's growth; only a split, which
//! rebuilds buckets and may double the directory, is charged by
//! re-summing the whole shell — and a split is one write in hundreds.

use super::{Bucket, SegMap};
use alloc::sync::Arc;
use core::mem::size_of;
use kevy_bytes::SmallBytes;
use kevy_hash::KevyHash;
use kevy_map::{RawEntryMut, malloc_footprint};

/// Bytes an `Arc<T>` box occupies: the two counts and the value.
#[inline]
pub(crate) fn arc_box<T>() -> u64 {
    malloc_footprint(2 * size_of::<usize>() + size_of::<T>()) as u64
}

impl<V: Clone> SegMap<V> {
    /// Bytes of everything but the entries' own heap: the directory, the
    /// bucket pointer array, and each bucket's box and table.
    pub(crate) fn shell_bytes(&self) -> u64 {
        let arrays = malloc_footprint(self.dirs.capacity() * size_of::<u32>())
            + malloc_footprint(self.buckets.capacity() * size_of::<Arc<Bucket<V>>>());
        arrays as u64
            + self
                .buckets
                .iter()
                .map(|b| arc_box::<Bucket<V>>() + b.map.footprint() as u64)
                .sum::<u64>()
    }

    /// [`SegMap::insert`], also answering by how many bytes the shell grew.
    pub(crate) fn insert_sized(&mut self, key: SmallBytes, value: V) -> (Option<V>, i64) {
        let slot = self.route(key.as_slice().kevy_hash());
        let bi = self.dirs[slot] as usize;
        let b = Arc::make_mut(&mut self.buckets[bi]);
        let (old, grown) = b.map.insert_sized(key, value);
        let mut grown = grown as i64;
        if old.is_none() {
            self.len += 1;
            if b.map.len() > super::BUCKET_SPLIT {
                let shell = self.shell_bytes() as i64;
                self.split(slot);
                grown += self.shell_bytes() as i64 - shell;
            }
        }
        (old, grown)
    }

    /// [`SegMap::remove`], also answering the heap bytes of the key it
    /// dropped (a table never shrinks, so nothing else moves).
    pub(crate) fn remove_keyed(&mut self, key: &[u8]) -> Option<(usize, V)> {
        let bi = self.bucket_of(key);
        let RawEntryMut::Occupied(e) = Arc::make_mut(&mut self.buckets[bi]).map.raw_entry_mut(key)
        else {
            return None;
        };
        let key_heap = e.key().heap_bytes();
        let v = e.remove();
        self.len -= 1;
        Some((key_heap, v))
    }
}

impl SegMap<SmallBytes> {
    /// [`crate::Value::weight`]'s SegHash arm: the outer box, the shell,
    /// and every field's and value's own heap.
    pub(crate) fn weight_as_hash(&self) -> u64 {
        let held = |b: &SmallBytes| malloc_footprint(b.heap_bytes()) as u64;
        arc_box::<Self>()
            + self.shell_bytes()
            + self.iter().map(|(f, v)| held(f) + held(v)).sum::<u64>()
    }
}
