//! Borrowing iterators over a [`KevyMap`] — `(&K, &V)`, `&K`-only, and
//! `&V`-only flavours.

use std::mem::MaybeUninit;

use crate::map::KevyMap;

/// `(&K, &V)` iterator over all live entries of a [`KevyMap`]; order unspecified.
pub struct Iter<'a, K, V> {
    metadata: &'a [u8],
    slots: &'a [MaybeUninit<(K, V)>],
    pos: usize,
}

impl<'a, K, V> Iter<'a, K, V> {
    /// Construct an iterator from a map's raw bucket slices.
    ///
    /// `metadata` may be longer than `slots` because the map keeps a
    /// trailing `GROUP_WIDTH - 1` byte mirror for SIMD-safe wraparound
    /// loads; only the first `slots.len()` metadata bytes correspond to
    /// real slots, and that's what we iterate over.
    pub(crate) fn new(metadata: &'a [u8], slots: &'a [MaybeUninit<(K, V)>]) -> Self {
        let real_len = slots.len();
        let metadata = &metadata[..real_len];
        Self {
            metadata,
            slots,
            pos: 0,
        }
    }

    /// Construct an iterator that starts at bucket `start` (clamped to
    /// `slots.len()`). Powers reservoir / random-start sampling for the
    /// `kevy-store` eviction sampler — chain two of these (start..end, then
    /// 0..start) to walk the table in a ring beginning at any position.
    pub(crate) fn with_start(
        metadata: &'a [u8],
        slots: &'a [MaybeUninit<(K, V)>],
        start: usize,
    ) -> Self {
        let real_len = slots.len();
        let metadata = &metadata[..real_len];
        Self {
            metadata,
            slots,
            pos: start.min(real_len),
        }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.metadata.len() {
            let i = self.pos;
            self.pos += 1;
            if self.metadata[i] & 0x80 == 0 {
                // SAFETY: full slot. The borrow's lifetime is tied to
                // self.slots: &'a [MaybeUninit<(K, V)>].
                let kv = unsafe { self.slots[i].assume_init_ref() };
                return Some((&kv.0, &kv.1));
            }
        }
        None
    }
}

impl<'a, K, V> IntoIterator for &'a KevyMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// `(&K, &mut V)` iterator over all live entries of a [`KevyMap`]; order
/// unspecified. Keys stay shared — mutating a key would corrupt its bucket.
pub struct IterMut<'a, K, V> {
    metadata: &'a [u8],
    slots: &'a mut [MaybeUninit<(K, V)>],
    pos: usize,
}

impl<'a, K, V> IterMut<'a, K, V> {
    /// Construct from a map's raw bucket slices (same mirror-tail trim as
    /// [`Iter::new`]).
    pub(crate) fn new(metadata: &'a [u8], slots: &'a mut [MaybeUninit<(K, V)>]) -> Self {
        let metadata = &metadata[..slots.len()];
        Self {
            metadata,
            slots,
            pos: 0,
        }
    }
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);
    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.metadata.len() {
            let i = self.pos;
            self.pos += 1;
            if self.metadata[i] & 0x80 == 0 {
                // SAFETY: full slot ⇒ initialised, and `pos` only advances, so
                // each index is yielded at most once — the returned `&mut V`s
                // are disjoint and all live within the `'a` borrow of `slots`.
                let kv = unsafe { &mut *(self.slots.as_mut_ptr().add(i) as *mut (K, V)) };
                return Some((&kv.0, &mut kv.1));
            }
        }
        None
    }
}

impl<'a, K, V> IntoIterator for &'a mut KevyMap<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

/// `&K` iterator over all live entries of a [`KevyMap`].
pub struct Keys<'a, K, V>(Iter<'a, K, V>);

impl<'a, K, V> Keys<'a, K, V> {
    pub(crate) fn new(inner: Iter<'a, K, V>) -> Self {
        Self(inner)
    }
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, _)| k)
    }
}

/// `&V` iterator over all live entries of a [`KevyMap`].
pub struct Values<'a, K, V>(Iter<'a, K, V>);

impl<'a, K, V> Values<'a, K, V> {
    pub(crate) fn new(inner: Iter<'a, K, V>) -> Self {
        Self(inner)
    }
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(_, v)| v)
    }
}
