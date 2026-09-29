//! Owning iteration over a [`KevyMap`] / [`crate::KevySet`], and the
//! equality both collections compare by.

use core::fmt;
use core::ptr;

use kevy_hash::KevyHash;

use crate::map::{EMPTY, KevyMap};

/// `(K, V)` iterator that consumes a [`KevyMap`]; order unspecified.
/// Entries not yet yielded are dropped with the iterator.
///
/// ```
/// let m: kevy_map::KevyMap<u64, &str> = [(1, "a"), (2, "b")].into_iter().collect();
/// let mut pairs: Vec<(u64, &str)> = m.into_iter().collect();
/// pairs.sort();
/// assert_eq!(pairs, [(1, "a"), (2, "b")]);
/// ```
pub struct IntoIter<K, V> {
    map: KevyMap<K, V>,
    pos: usize,
}

impl<K, V> IntoIter<K, V> {
    pub(crate) fn new(map: KevyMap<K, V>) -> Self {
        IntoIter { map, pos: 0 }
    }
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        while self.pos < self.map.cap {
            let i = self.pos;
            self.pos += 1;
            // SAFETY: i < cap, so the metadata byte is in bounds.
            let meta = unsafe { self.map.metadata_ptr.as_ptr().add(i) };
            // SAFETY: as above; the byte is initialised from allocation on.
            if unsafe { *meta } & 0x80 == 0 {
                // the slot is marked empty before its entry leaves, so the
                // map's Drop never sees it again: moved out exactly once
                // SAFETY: in bounds as above.
                unsafe { *meta = EMPTY };
                self.map.occupied -= 1;
                // SAFETY: a full slot holds an initialised (K, V).
                return Some(unsafe { ptr::read(self.map.slots_ptr.as_ptr().add(i).cast()) });
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.map.occupied, Some(self.map.occupied))
    }
}

impl<K, V> ExactSizeIterator for IntoIter<K, V> {}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for IntoIter<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntoIter").field("remaining", &self.map.occupied).finish_non_exhaustive()
    }
}

impl<K, V> IntoIterator for KevyMap<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;

    fn into_iter(self) -> IntoIter<K, V> {
        IntoIter::new(self)
    }
}

/// Two maps are equal when they hold the same keys, each with an equal
/// value — capacity and probe layout do not matter.
///
/// ```
/// use kevy_map::KevyMap;
/// let a: KevyMap<u64, u8> = [(1, 1), (2, 2)].into_iter().collect();
/// let mut b = KevyMap::with_capacity(64);
/// b.insert(2, 2);
/// b.insert(1, 1);
/// assert_eq!(a, b);
/// b.insert(3, 3);
/// assert_ne!(a, b);
/// ```
impl<K, V> PartialEq for KevyMap<K, V>
where
    K: KevyHash + Eq,
    V: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl<K: KevyHash + Eq, V: Eq> Eq for KevyMap<K, V> {}
