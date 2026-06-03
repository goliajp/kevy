//! Trait impls for [`SmallBytes`] that only need the public byte-slice
//! view (`as_slice()`). The discriminator-aware `PartialEq` / `Eq` stay
//! in `lib.rs` next to the union definition because they reach into
//! `self.inline` / `self.heap` directly for the same-variant fast paths.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::SmallBytes;

impl fmt::Debug for SmallBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Match Vec<u8>'s Debug ("[1, 2, 3]" form).
        f.debug_list().entries(self.as_slice().iter()).finish()
    }
}

impl PartialOrd for SmallBytes {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmallBytes {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}

impl Hash for SmallBytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl AsRef<[u8]> for SmallBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::borrow::Borrow<[u8]> for SmallBytes {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

/// `KevyHash` agrees with the byte-slice impl, so a `KevyMap<SmallBytes, V>`
/// can be queried with `&[u8]` (via `Borrow<[u8]>`) and the hash matches.
impl kevy_hash::KevyHash for SmallBytes {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        self.as_slice().kevy_hash()
    }
}

impl From<&[u8]> for SmallBytes {
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl From<Vec<u8>> for SmallBytes {
    fn from(vec: Vec<u8>) -> Self {
        Self::from_vec(vec)
    }
}
