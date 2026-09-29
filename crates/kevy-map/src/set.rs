//! `HashSet`-shaped wrapper over [`KevyMap<K, ()>`] — same per-shard /
//! single-trust-domain assumptions; exposes the underlying map's bucket-addr
//! API via [`KevySet::as_map`].

use core::borrow::Borrow;
use core::fmt;

use kevy_hash::KevyHash;

use crate::into_iter::IntoIter;
use crate::iter::Iter;
use crate::map::KevyMap;

/// `HashSet`-shaped wrapper over [`KevyMap<K, ()>`].
///
/// Carries the same per-shard / single-trust-domain assumptions; differs from
/// `std::HashSet` only by hashing through [`KevyHash`] (one-call inlinable)
/// and exposing the underlying `KevyMap`'s bucket-address API via
/// [`KevySet::as_map`] for callers that want prefetch.
///
/// ```
/// use kevy_map::KevySet;
/// let mut s: KevySet<Vec<u8>> = KevySet::new();
/// assert!(s.insert(b"a".to_vec()));
/// assert!(!s.insert(b"a".to_vec()), "already a member");
/// assert!(s.contains(b"a".as_slice()));
/// assert!(s.remove(b"a".as_slice()));
/// assert!(s.is_empty());
/// ```
#[derive(Clone)]
pub struct KevySet<K>(KevyMap<K, ()>);

impl<K> KevySet<K> {
    /// Construct an empty set without allocating.
    pub fn new() -> Self {
        Self(KevyMap::new())
    }

    /// Construct a set sized for `cap_hint` members without growing.
    pub fn with_capacity(cap_hint: usize) -> Self {
        Self(KevyMap::with_capacity(cap_hint))
    }

    /// Live member count.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Whether `len() == 0`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Allocated slot count of the underlying map.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
    /// Drop every member and reset the metadata. Keeps the allocation.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// `&K` iterator over all members (unspecified order).
    pub fn iter(&self) -> SetIter<'_, K> {
        SetIter(self.0.iter())
    }

    /// An iterator that begins at slot `start` and wraps once around the whole
    /// table. Take the first element for an arbitrary member in O(1) expected
    /// time — the pattern SPOP and SRANDMEMBER need, and the one Redis's
    /// `dictGetRandomKey` uses.
    ///
    /// Not perfectly uniform: a member sitting after a long run of empty slots
    /// is likelier to be picked than one in a dense cluster. Redis has the same
    /// bias for the same reason, and the contract is "arbitrary", not "uniform".
    /// What it is NOT is the identical member every single time, which is what
    /// `iter().next()` gives you.
    pub fn iter_from_slot(&self, start: usize) -> impl Iterator<Item = &K> {
        let cap = self.0.capacity();
        let start = if cap == 0 { 0 } else { start % cap };
        self.0.iter_from_bucket(start).chain(self.0.iter().take(start)).map(|(k, ())| k)
    }

    /// The backing map, for callers that want its bucket addresses (prefetch).
    pub fn as_map(&self) -> &KevyMap<K, ()> {
        &self.0
    }
}

impl<K: KevyHash + Eq> KevySet<K> {
    /// Insert `key`. Returns `true` if newly added, `false` if it was already
    /// present (matches `HashSet::insert`).
    pub fn insert(&mut self, key: K) -> bool {
        self.0.insert(key, ()).is_none()
    }
}

impl<K> KevySet<K> {
    /// Whether `key` is a member of the set.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        self.0.contains_key(key)
    }

    /// Remove `key`; returns `true` if it was present (matches `HashSet::remove`).
    pub fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        self.0.remove(key).is_some()
    }
}

impl<K> Default for KevySet<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: fmt::Debug> fmt::Debug for KevySet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// `&K` iterator over all members of a [`KevySet`]; order unspecified.
///
/// ```
/// let s: kevy_map::KevySet<u64> = [2, 4].into_iter().collect();
/// let mut v: Vec<u64> = s.iter().copied().collect();
/// v.sort();
/// assert_eq!(v, [2, 4]);
/// ```
#[derive(Debug)]
pub struct SetIter<'a, K>(Iter<'a, K, ()>);

/// `K` iterator that consumes a [`KevySet`]; order unspecified.
///
/// ```
/// let s: kevy_map::KevySet<u64> = [3, 1].into_iter().collect();
/// let mut v: Vec<u64> = s.into_iter().collect();
/// v.sort();
/// assert_eq!(v, [1, 3]);
/// ```
#[derive(Debug)]
pub struct SetIntoIter<K>(IntoIter<K, ()>);

impl<K> Iterator for SetIntoIter<K> {
    type Item = K;
    fn next(&mut self) -> Option<K> {
        self.0.next().map(|(k, ())| k)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<K> ExactSizeIterator for SetIntoIter<K> {}

impl<K> IntoIterator for KevySet<K> {
    type Item = K;
    type IntoIter = SetIntoIter<K>;
    fn into_iter(self) -> SetIntoIter<K> {
        SetIntoIter(self.0.into_iter())
    }
}

/// Two sets are equal when they hold the same members.
///
/// ```
/// let a: kevy_map::KevySet<u64> = [1, 2].into_iter().collect();
/// let b: kevy_map::KevySet<u64> = [2, 1].into_iter().collect();
/// assert_eq!(a, b);
/// ```
impl<K: KevyHash + Eq> PartialEq for KevySet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<K: KevyHash + Eq> Eq for KevySet<K> {}

impl<'a, K> Iterator for SetIter<'a, K> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, ())| k)
    }
}

impl<'a, K> IntoIterator for &'a KevySet<K> {
    type Item = &'a K;
    type IntoIter = SetIter<'a, K>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: KevyHash + Eq> FromIterator<K> for KevySet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let mut s = match iter.size_hint() {
            (lo, Some(hi)) if hi <= lo.saturating_mul(2) => Self::with_capacity(hi),
            (lo, _) => Self::with_capacity(lo),
        };
        for k in iter {
            s.insert(k);
        }
        s
    }
}

impl<K: KevyHash + Eq> Extend<K> for KevySet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        for k in iter {
            self.insert(k);
        }
    }
}
