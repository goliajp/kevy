//! `kevy-map` — a purpose-built open-addressing hashtable for kevy's keyspace.
//!
//! Per-shard, single-threaded, single-trust-domain. Trades `std::HashMap`'s
//! generality for three kevy-specific wins:
//!
//! 1. **Bucket-address API** (`prefetch_for_hash`, future) — exposes the
//!    table's bucket metadata pointer so the command-batch driver can
//!    `prefetcht0` the next command's group while finishing the current.
//! 2. **No DoS-hardening tax** — single trust domain ⇒ no random seed.
//!    Hasher is `kevy_hash::KevyHash` (one-call inlinable).
//! 3. **Cache-conscious layout** — Swiss-style metadata bytes scanned (scalar
//!    in this commit; SSE2 group scan lands in v0.metal-4+5 step 6); slots
//!    AoS so the post-match key+value read hits one cache line.
//!
//! Design RFC: `rfcs/2026-05-26-kevy-map-design.md`.
//!
//! Charter: pure Rust, no `crates.io` deps; `unsafe` is allowed here (scoped
//! to this crate) so `kevy-store` keeps `forbid(unsafe_code)`.

#![deny(unsafe_op_in_unsafe_fn)]

pub use kevy_hash::KevyHash;

use std::borrow::Borrow;
use std::fmt;
use std::mem::MaybeUninit;
use std::ptr;

/// Metadata byte for an empty slot (top bit set, value bits 1's — distinct
/// from DELETED so the probe loop can stop at EMPTY but skip DELETED).
const EMPTY: u8 = 0xFF;
/// Metadata byte for a tombstone (top bit set, value bits 0).
const DELETED: u8 = 0x80;
/// Minimum table size. ≥ 16 (one SSE2 group) so the future SIMD path can run
/// a full group scan unconditionally.
const MIN_CAP: usize = 16;

/// Top-7 bits of the hash, used as the per-slot metadata byte for occupied
/// slots. The top bit is always 0 (so occupancy = `meta & 0x80 == 0`).
#[inline]
fn h2(hash: u64) -> u8 {
    ((hash >> 57) & 0x7F) as u8
}

/// Issue a hint to fetch the cache line containing `ptr` into L1 ("T0" =
/// "all levels"). Stable on x86_64 / aarch64; no-op elsewhere.
#[inline(always)]
fn prefetch_t0(ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: _mm_prefetch reads no memory; any aligned/unaligned/
        // out-of-bounds pointer is permitted by the ISA.
        unsafe {
            core::arch::x86_64::_mm_prefetch(
                ptr as *const i8,
                core::arch::x86_64::_MM_HINT_T0,
            );
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: prfm reads no memory; any pointer permitted.
        unsafe {
            core::arch::asm!(
                "prfm pldl1keep, [{p}]",
                p = in(reg) ptr,
                options(nostack, preserves_flags, readonly),
            );
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = ptr;
    }
}

/// An open-addressing Swiss-style hashtable keyed by [`KevyHash`].
///
/// Power-of-two capacity (`mask = cap - 1`); 7/8 load factor; linear probing
/// over the metadata array; full slots' (K, V) live AoS in a parallel slot
/// array of `MaybeUninit<(K, V)>`.
pub struct KevyMap<K, V> {
    /// One byte per slot. `EMPTY` (0xFF), `DELETED` (0x80), or `h2` (0x00..=0x7F).
    metadata: Box<[u8]>,
    /// One slot per metadata byte. Initialised iff the corresponding metadata
    /// byte is in 0x00..=0x7F (i.e. `meta & 0x80 == 0`).
    slots: Box<[MaybeUninit<(K, V)>]>,
    /// `cap - 1`; 0 when the table is uninitialised (cap = 0).
    mask: usize,
    /// Live entries.
    occupied: usize,
    /// Tombstones (not yet reclaimed).
    deleted: usize,
}

enum ProbeOutcome {
    Found(usize),
    NotFound {
        insert_at: usize,
        via_tombstone: bool,
    },
}

impl<K, V> KevyMap<K, V> {
    /// Construct an empty map without allocating.
    pub fn new() -> Self {
        Self {
            metadata: Box::new([]),
            slots: Box::new_uninit_slice(0),
            mask: 0,
            occupied: 0,
            deleted: 0,
        }
    }

    /// Construct a map sized to hold `cap_hint` entries without growing
    /// (accounting for the 7/8 load factor).
    pub fn with_capacity(cap_hint: usize) -> Self {
        if cap_hint == 0 {
            return Self::new();
        }
        // ceil(cap_hint * 8 / 7) → smallest table where cap_hint fits below 7/8.
        let needed = cap_hint.saturating_mul(8).div_ceil(7);
        let cap = needed.next_power_of_two().max(MIN_CAP);
        Self::alloc_table(cap)
    }

    fn alloc_table(cap: usize) -> Self {
        debug_assert!(cap.is_power_of_two());
        debug_assert!(cap >= MIN_CAP);
        let metadata = vec![EMPTY; cap].into_boxed_slice();
        let slots: Box<[MaybeUninit<(K, V)>]> = Box::new_uninit_slice(cap);
        // v0.metal-9: hint THP on both backing arrays. madvise tolerates
        // mis-alignment by no-op'ing (Linux EINVAL is silenced inside
        // kevy_sys::advise_hugepage). On 10M+ key tables the metadata array
        // alone is 16 MB — well over the 2 MB HP boundary, so the kernel's
        // khugepaged can promote it in place. Cheap on the non-Linux paths
        // (compile-time no-op).
        kevy_sys::advise_hugepage(metadata.as_ptr(), metadata.len());
        kevy_sys::advise_hugepage(
            slots.as_ptr() as *const u8,
            std::mem::size_of_val::<[_]>(&slots),
        );
        Self {
            metadata,
            slots,
            mask: cap - 1,
            occupied: 0,
            deleted: 0,
        }
    }

    /// Live entry count.
    #[inline]
    pub fn len(&self) -> usize {
        self.occupied
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    /// Allocated slot count (NOT live entries).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.metadata.len()
    }

    /// Drop every live entry and reset the metadata. Keeps the allocation.
    pub fn clear(&mut self) {
        if std::mem::needs_drop::<(K, V)>() {
            for (i, m) in self.metadata.iter().enumerate() {
                if *m & 0x80 == 0 {
                    // SAFETY: full slot ⇒ initialised.
                    unsafe { ptr::drop_in_place(self.slots[i].as_mut_ptr()) };
                }
            }
        }
        for m in self.metadata.iter_mut() {
            *m = EMPTY;
        }
        self.occupied = 0;
        self.deleted = 0;
    }

    /// `(&K, &V)` over all live entries; order is unspecified.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            metadata: &self.metadata,
            slots: &self.slots,
            pos: 0,
        }
    }

    /// `&K` over all live entries.
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys(self.iter())
    }

    /// `&V` over all live entries.
    pub fn values(&self) -> Values<'_, K, V> {
        Values(self.iter())
    }

    /// Hint the CPU to fetch the bucket cache line that a probe at `hash`
    /// would start at. The v0.metal-5 lever against the bucket-probe DRAM
    /// miss: the command-batch driver calls this for command N+1 while
    /// finishing command N, so by the time N+1 actually probes the
    /// metadata, the line is in L1.
    ///
    /// No-op when the table is empty. Cheap when not empty (a single
    /// `prefetcht0` on x86_64 / `prfm pldl1keep` on aarch64; a regular
    /// volatile load on other arches via [`std::intrinsics`] — but we
    /// only use stable intrinsics here, so non-x86/aarch64 architectures
    /// degrade to a no-op rather than a fake hint).
    #[inline(always)]
    pub fn prefetch_for_hash(&self, hash: u64) {
        let cap = self.metadata.len();
        if cap == 0 {
            return;
        }
        let mask = cap - 1;
        let idx = (hash as usize) & mask;
        // SAFETY: idx < cap = metadata.len() ⇒ ptr in-bounds; prefetch reads
        // never trap and never observe values.
        let ptr = unsafe { self.metadata.as_ptr().add(idx) };
        prefetch_t0(ptr);
    }

    #[inline]
    fn cap(&self) -> usize {
        self.metadata.len()
    }

    /// 7/8 of the capacity — the inclusive max for `occupied + deleted`.
    #[inline]
    fn threshold(&self) -> usize {
        let c = self.cap();
        c - (c / 8)
    }
}

impl<K: KevyHash + Eq, V> KevyMap<K, V> {
    /// Insert `(key, value)`. Returns the old value if `key` was already
    /// present. Following `std::HashMap` semantics, the existing K is kept on
    /// overwrite — only V is replaced.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.maybe_grow();
        let hash = key.kevy_hash();
        match self.probe_with_key(hash, &key) {
            ProbeOutcome::Found(idx) => {
                // SAFETY: slot is full ⇒ initialised. We replace only the V
                // field; the old K is kept (std HashMap semantics).
                let v_ptr = unsafe {
                    let kv: *mut (K, V) = self.slots[idx].as_mut_ptr();
                    ptr::addr_of_mut!((*kv).1)
                };
                let old_v = unsafe { ptr::replace(v_ptr, value) };
                drop(key);
                Some(old_v)
            }
            ProbeOutcome::NotFound {
                insert_at,
                via_tombstone,
            } => {
                self.metadata[insert_at] = h2(hash);
                self.slots[insert_at].write((key, value));
                self.occupied += 1;
                if via_tombstone {
                    self.deleted -= 1;
                }
                None
            }
        }
    }

    fn maybe_grow(&mut self) {
        if self.cap() == 0 || (self.occupied + self.deleted) >= self.threshold() {
            self.grow();
        }
    }

    fn grow(&mut self) {
        let new_cap = if self.cap() == 0 {
            MIN_CAP
        } else {
            self.cap()
                .checked_mul(2)
                .expect("kevy-map: capacity doubling overflow")
        };
        let mut new_map = Self::alloc_table(new_cap);
        // Move every live entry over. After ptr::read'ing a slot we mark its
        // metadata DELETED, so any subsequent Drop (incl. panic unwind) won't
        // double-free; the old allocation will free with all-DELETED metadata.
        for i in 0..self.metadata.len() {
            if self.metadata[i] & 0x80 == 0 {
                // SAFETY: full slot ⇒ initialised; we mark DELETED immediately
                // so this byte is never re-read as occupied.
                let (k, v) = unsafe { ptr::read(self.slots[i].as_ptr()) };
                self.metadata[i] = DELETED;
                let hash = k.kevy_hash();
                new_map.insert_known_unique(hash, k, v);
            }
        }
        // All occupied entries are now in new_map; the old self has no live
        // slots.
        self.occupied = 0;
        self.deleted = 0;
        std::mem::swap(self, &mut new_map);
        // new_map (now the old self) drops; metadata is all DELETED ⇒ Drop
        // walks but touches no slots.
    }

    /// Insert under the assumption that the key isn't already present (used
    /// by `grow` to repopulate the new table). Skips the duplicate-key check.
    fn insert_known_unique(&mut self, hash: u64, k: K, v: V) {
        let h2v = h2(hash);
        let mut probe = (hash as usize) & self.mask;
        loop {
            if self.metadata[probe] == EMPTY {
                self.metadata[probe] = h2v;
                self.slots[probe].write((k, v));
                self.occupied += 1;
                return;
            }
            // grow's destination is always fresh ⇒ EMPTY is reachable.
            probe = (probe + 1) & self.mask;
        }
    }

    fn probe_with_key(&self, hash: u64, key: &K) -> ProbeOutcome {
        if self.cap() == 0 {
            return ProbeOutcome::NotFound {
                insert_at: 0,
                via_tombstone: false,
            };
        }
        let h2v = h2(hash);
        let mut probe = (hash as usize) & self.mask;
        let mut first_deleted: Option<usize> = None;
        loop {
            let m = self.metadata[probe];
            if m == EMPTY {
                return ProbeOutcome::NotFound {
                    insert_at: first_deleted.unwrap_or(probe),
                    via_tombstone: first_deleted.is_some(),
                };
            }
            if m == DELETED {
                if first_deleted.is_none() {
                    first_deleted = Some(probe);
                }
            } else if m == h2v {
                // SAFETY: full slot ⇒ initialised.
                let kv = unsafe { self.slots[probe].assume_init_ref() };
                if &kv.0 == key {
                    return ProbeOutcome::Found(probe);
                }
            }
            probe = (probe + 1) & self.mask;
        }
    }
}

impl<K, V> KevyMap<K, V> {
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        // SAFETY: find_by_borrow only returns indices into full slots.
        let kv = unsafe { self.slots[idx].assume_init_ref() };
        Some(&kv.1)
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        // SAFETY: full slot.
        let kv = unsafe { self.slots[idx].assume_init_mut() };
        Some(&mut kv.1)
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        self.find_by_borrow(key).is_some()
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        self.metadata[idx] = DELETED;
        self.occupied -= 1;
        self.deleted += 1;
        // SAFETY: slot was full, we just marked it DELETED so it won't be
        // read again; ptr::read moves the (K, V) out.
        let (_k, v) = unsafe { ptr::read(self.slots[idx].as_ptr()) };
        Some(v)
    }

    fn find_by_borrow<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        if self.cap() == 0 {
            return None;
        }
        let hash = key.kevy_hash();
        let h2v = h2(hash);
        let mut probe = (hash as usize) & self.mask;
        loop {
            let m = self.metadata[probe];
            if m == EMPTY {
                return None;
            }
            if m == h2v {
                // SAFETY: full slot.
                let kv = unsafe { self.slots[probe].assume_init_ref() };
                if kv.0.borrow() == key {
                    return Some(probe);
                }
            }
            probe = (probe + 1) & self.mask;
        }
    }
}

impl<K, V> Default for KevyMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Drop for KevyMap<K, V> {
    fn drop(&mut self) {
        if std::mem::needs_drop::<(K, V)>() {
            for (i, m) in self.metadata.iter().enumerate() {
                if *m & 0x80 == 0 {
                    // SAFETY: full slot ⇒ initialised.
                    unsafe { ptr::drop_in_place(self.slots[i].as_mut_ptr()) };
                }
            }
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for KevyMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

// ---- Iter ----------------------------------------------------------------

pub struct Iter<'a, K, V> {
    metadata: &'a [u8],
    slots: &'a [MaybeUninit<(K, V)>],
    pos: usize,
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

pub struct Keys<'a, K, V>(Iter<'a, K, V>);
impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, _)| k)
    }
}

pub struct Values<'a, K, V>(Iter<'a, K, V>);
impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(_, v)| v)
    }
}

impl<K: KevyHash + Eq, V> FromIterator<(K, V)> for KevyMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let mut m = match iter.size_hint() {
            (lo, Some(hi)) if hi <= lo.saturating_mul(2) => Self::with_capacity(hi),
            (lo, _) => Self::with_capacity(lo),
        };
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

impl<K: KevyHash + Eq, V> Extend<(K, V)> for KevyMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

// ---- KevySet -------------------------------------------------------------

/// `HashSet`-shaped wrapper over [`KevyMap<K, ()>`].
///
/// Carries the same per-shard / single-trust-domain assumptions; differs from
/// `std::HashSet` only by hashing through [`KevyHash`] (one-call inlinable)
/// and exposing the underlying `KevyMap`'s bucket-address API via
/// [`KevySet::as_map`] for callers that want prefetch.
pub struct KevySet<K>(KevyMap<K, ()>);

impl<K> KevySet<K> {
    pub fn new() -> Self {
        Self(KevyMap::new())
    }

    pub fn with_capacity(cap_hint: usize) -> Self {
        Self(KevyMap::with_capacity(cap_hint))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    #[inline]
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// `&K` iterator over all members (unspecified order).
    pub fn iter(&self) -> SetIter<'_, K> {
        SetIter(self.0.iter())
    }

    /// Borrow the underlying map (gives access to the bucket-addr / prefetch
    /// API once added in v0.metal-5).
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

pub struct SetIter<'a, K>(Iter<'a, K, ()>);

impl<'a, K> Iterator for SetIter<'a, K> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, _)| k)
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

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn new_is_empty() {
        let m: KevyMap<u64, u64> = KevyMap::new();
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert_eq!(m.capacity(), 0);
        assert_eq!(m.get(&5u64), None);
        assert!(!m.contains_key(&5u64));
    }

    #[test]
    fn insert_get() {
        let mut m = KevyMap::<u64, u64>::new();
        assert!(m.insert(1, 10).is_none());
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&1u64), Some(&10));
        assert_eq!(m.get(&2u64), None);
        assert!(m.contains_key(&1u64));
    }

    #[test]
    fn insert_duplicate_replaces_value_returns_old() {
        let mut m = KevyMap::<u64, u64>::new();
        assert_eq!(m.insert(1, 10), None);
        assert_eq!(m.insert(1, 20), Some(10));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&1u64), Some(&20));
    }

    #[test]
    fn remove_returns_value_decreases_len() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 10);
        m.insert(2, 20);
        assert_eq!(m.remove(&1u64), Some(10));
        assert_eq!(m.len(), 1);
        assert_eq!(m.remove(&1u64), None);
        assert_eq!(m.get(&1u64), None);
        assert_eq!(m.get(&2u64), Some(&20));
    }

    #[test]
    fn tombstone_reused_on_reinsert() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 10);
        m.remove(&1u64);
        assert_eq!(m.len(), 0);
        m.insert(1, 30);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&1u64), Some(&30));
    }

    #[test]
    fn grow_preserves_all_entries_10k() {
        let mut m = KevyMap::<u64, u64>::new();
        for i in 0..10_000u64 {
            m.insert(i, i.wrapping_mul(7));
        }
        assert_eq!(m.len(), 10_000);
        for i in 0..10_000u64 {
            assert_eq!(m.get(&i), Some(&i.wrapping_mul(7)));
        }
    }

    #[test]
    fn byte_string_keys_with_borrow_lookup() {
        let mut m = KevyMap::<Vec<u8>, u64>::new();
        m.insert(b"foo".to_vec(), 1);
        m.insert(b"bar".to_vec(), 2);
        assert_eq!(m.get(b"foo".as_slice()), Some(&1));
        assert_eq!(m.get(b"missing".as_slice()), None);
        assert_eq!(m.remove(b"bar".as_slice()), Some(2));
        assert_eq!(m.len(), 1);
        assert!(!m.contains_key(b"bar".as_slice()));
    }

    #[test]
    fn iter_yields_all_entries() {
        let mut m = KevyMap::<u64, u64>::new();
        for i in 0..20u64 {
            m.insert(i, i + 100);
        }
        let mut seen: Vec<(u64, u64)> = m.iter().map(|(&k, &v)| (k, v)).collect();
        seen.sort();
        let expected: Vec<(u64, u64)> = (0..20).map(|i| (i, i + 100)).collect();
        assert_eq!(seen, expected);
    }

    struct DropCount<'a>(&'a Cell<usize>);
    impl Drop for DropCount<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn clear_drops_entries_and_resets_len() {
        let counter = Cell::new(0);
        let mut m: KevyMap<u64, DropCount<'_>> = KevyMap::new();
        for i in 0..50u64 {
            m.insert(i, DropCount(&counter));
        }
        assert_eq!(m.len(), 50);
        m.clear();
        assert_eq!(m.len(), 0);
        assert_eq!(counter.get(), 50);
        assert!(m.capacity() >= 50);
        m.insert(0, DropCount(&counter));
        assert_eq!(m.len(), 1);
        drop(m);
        assert_eq!(counter.get(), 51);
    }

    #[test]
    fn drop_runs_for_remaining_entries() {
        let counter = Cell::new(0);
        {
            let mut m: KevyMap<u64, DropCount<'_>> = KevyMap::new();
            for i in 0..30u64 {
                m.insert(i, DropCount(&counter));
            }
            m.remove(&5u64);
            assert_eq!(counter.get(), 1);
        }
        assert_eq!(counter.get(), 30);
    }

    #[test]
    fn grow_then_remove_then_grow_again_stays_consistent() {
        let mut m = KevyMap::<u64, u64>::new();
        for i in 0..2000u64 {
            m.insert(i, i);
        }
        for i in 0..1000u64 {
            assert_eq!(m.remove(&i), Some(i));
        }
        for i in 2000..4000u64 {
            m.insert(i, i);
        }
        assert_eq!(m.len(), 3000);
        for i in 1000..4000u64 {
            assert_eq!(m.get(&i), Some(&i));
        }
        for i in 0..1000u64 {
            assert_eq!(m.get(&i), None);
        }
    }

    #[test]
    fn with_capacity_preallocates() {
        let m: KevyMap<u64, u64> = KevyMap::with_capacity(100);
        // ceil(100 * 8 / 7) = 115 → next_pow2 = 128
        assert_eq!(m.capacity(), 128);
        let m: KevyMap<u64, u64> = KevyMap::with_capacity(0);
        assert_eq!(m.capacity(), 0);
        let m: KevyMap<u64, u64> = KevyMap::with_capacity(1);
        assert_eq!(m.capacity(), MIN_CAP);
    }

    #[test]
    fn get_mut_allows_mutation() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 10);
        *m.get_mut(&1u64).unwrap() = 20;
        assert_eq!(m.get(&1u64), Some(&20));
        assert!(m.get_mut(&2u64).is_none());
    }

    #[test]
    fn debug_format_matches_map_shape() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 10);
        m.insert(2, 20);
        let s = format!("{m:?}");
        // Order is unspecified but both entries must appear and the shape is a map.
        assert!(s.starts_with('{'));
        assert!(s.ends_with('}'));
        assert!(s.contains("1: 10") || s.contains("1:10"));
        assert!(s.contains("2: 20") || s.contains("2:20"));
    }

    #[test]
    fn into_iter_ref_works() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 10);
        m.insert(2, 20);
        let mut total = 0u64;
        for (k, v) in &m {
            total += *k + *v;
        }
        assert_eq!(total, 1 + 10 + 2 + 20);
    }

    #[test]
    fn many_collisions_via_long_byte_keys() {
        // Stresses the linear probing loop on a real keyspace shape (variable-
        // length byte keys; the hasher avalanches via fmix64 so h2 distribution
        // is uniform — exercises real-world probe chains rather than a
        // degenerate collision storm).
        let mut m = KevyMap::<Vec<u8>, u64>::new();
        let n = 5_000u64;
        for i in 0..n {
            let k = format!("session:{i:08}:user").into_bytes();
            m.insert(k, i);
        }
        assert_eq!(m.len(), n as usize);
        for i in 0..n {
            let k = format!("session:{i:08}:user");
            assert_eq!(m.get(k.as_bytes()), Some(&i));
        }
    }

    #[test]
    fn zst_value_type() {
        let mut m = KevyMap::<u64, ()>::new();
        assert!(m.insert(1, ()).is_none());
        assert!(m.insert(1, ()).is_some());
        assert!(m.contains_key(&1u64));
        assert_eq!(m.remove(&1u64), Some(()));
    }

    #[test]
    fn set_basic_ops() {
        let mut s: KevySet<Vec<u8>> = KevySet::new();
        assert!(s.insert(b"a".to_vec()));
        assert!(!s.insert(b"a".to_vec())); // duplicate ⇒ false
        assert_eq!(s.len(), 1);
        assert!(s.contains(b"a".as_slice()));
        assert!(!s.contains(b"b".as_slice()));
        assert!(s.remove(b"a".as_slice()));
        assert!(!s.remove(b"a".as_slice()));
        assert!(s.is_empty());
    }

    #[test]
    fn set_iter_yields_members() {
        let mut s: KevySet<u64> = KevySet::new();
        for i in 0..10u64 {
            s.insert(i);
        }
        let mut got: Vec<u64> = s.iter().copied().collect();
        got.sort();
        assert_eq!(got, (0..10u64).collect::<Vec<_>>());
    }

    #[test]
    fn prefetch_for_hash_is_safe_on_any_state() {
        // Just exercise the API on empty and populated tables; it's a hint
        // with no observable side effect, so we can only test it doesn't
        // panic / miscompile.
        let m: KevyMap<u64, u64> = KevyMap::new();
        m.prefetch_for_hash(0);
        m.prefetch_for_hash(u64::MAX);
        let mut m = KevyMap::<u64, u64>::new();
        for i in 0..50u64 {
            m.insert(i, i);
        }
        for i in 0..50u64 {
            m.prefetch_for_hash(i.kevy_hash());
        }
    }

    #[test]
    fn capacity_grows_doubling_from_min_cap() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 1);
        assert_eq!(m.capacity(), MIN_CAP);
        // Fill to just past 7/8 of MIN_CAP: threshold = 14
        for i in 2..=14u64 {
            m.insert(i, i);
        }
        // 14 entries, threshold = 14, next insert grows.
        m.insert(15, 15);
        assert_eq!(m.capacity(), MIN_CAP * 2);
    }
}
