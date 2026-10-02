//! Bucket-sharded map — element-granularity COW for giant hashes/sets.
//!
//! A hash or set past [`HS_PROMOTE`] elements stops being one
//! `Arc<KevyMap>` and becomes a [`SegMap`]: an extendible-hash
//! directory over `Arc`-shared buckets. A snapshot/rewrite view pins
//! the whole structure by cloning the outer Arc (sharing every
//! bucket); the first write during that window clones the outer shell
//! (index + pointer arrays) plus ONLY the bucket it routes to —
//! bounding the COW stall to one bucket (≤ [`BUCKET_SPLIT`] entries)
//! instead of the whole value. The list twin is `list_seg.rs`; the
//! design rationale lives in the element-COW RFC.
//!
//! Layout note: the directory holds bucket INDICES (`Vec<u32>`), and
//! buckets live once each in a side vector — so an `Arc<Bucket>` is
//! shared only with snapshot views, never with sibling directory
//! slots. `Arc::make_mut` therefore means exactly "unshare from
//! views"; the first cut of this file shared one `Arc` across the
//! slots of an unsplit range, and a write through a non-canonical slot
//! silently forked the bucket.
//!
//! Routing uses the TOP bits of the same [`kevy_hash::KevyHash`] the
//! buckets use internally (which mask with LOW bits) — the two index
//! spaces stay independent. Buckets split locally (per-bucket local
//! depth; the directory doubles only when the splitting bucket is at
//! global depth), so no write ever rehashes the whole map: split work
//! is one bucket, directory doubling is an index-array copy.
//!
//! ```
//! use kevy_store::SmallBytes;
//! use kevy_store::seg_map::SegMap;
//! let mut m: SegMap<()> = SegMap::default();
//! for i in 0..2_000u32 {
//!     m.insert(SmallBytes::from_slice(&i.to_be_bytes()), ());
//! }
//! let view = m.clone(); // shares every bucket
//! m.remove(&7u32.to_be_bytes());
//! assert_eq!(view.len(), 2_000);
//! assert_eq!(m.len(), 1_999);
//! ```

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::sync::Arc;
use kevy_bytes::SmallBytes;
use kevy_hash::KevyHash;
use kevy_map::KevyMap;

/// Bucket size that triggers a split. One bucket is the per-WRITE COW
/// clone bound — and, because a burst of hash-scattered writes under a
/// pinned view first-touches many buckets in one reactor tick, it is
/// also the granularity the per-TICK aggregate spreads over (the total
/// bytes ≈ the value, whatever the granularity — same as fork-based
/// page COW; finer buckets amortize the spikes). 2K entries ≈ ~130 KB
/// per clone at 2K. The closing soak sized it empirically: 16K buckets
/// aggregated a scattered burst to ~1.9 s ticks, 2K to a 188 ms
/// worst tick, 512 (~33 KB per clone) holds the window-opening burst
/// under the 100 ms tick bar with the control run's ~50 ms noise floor.
///
/// ```
/// use kevy_store::SmallBytes;
/// use kevy_store::seg_map::{BUCKET_SPLIT, SegMap};
/// let mut m: SegMap<()> = SegMap::default();
/// for i in 0..4 * BUCKET_SPLIT as u32 {
///     m.insert(SmallBytes::from_slice(&i.to_be_bytes()), ());
/// }
/// // past the split size the map has spread over buckets; every key still resolves
/// assert!((0..4 * BUCKET_SPLIT as u32).all(|i| m.contains_key(&i.to_be_bytes())));
/// ```
pub const BUCKET_SPLIT: usize = 512;
/// Flat `Value::Hash`/`Value::Set` length at which a write promotes to
/// the sharded representation.
///
/// ```
/// use kevy_store::Store;
/// use kevy_store::seg_map::HS_PROMOTE;
/// let mut s = Store::new();
/// let members: Vec<Vec<u8>> = (0..=HS_PROMOTE).map(|i| i.to_string().into_bytes()).collect();
/// let refs: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
/// assert_eq!(s.sadd(b"big", &refs)?, HS_PROMOTE + 1);
/// assert_eq!(s.scard(b"big")?, HS_PROMOTE + 1);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
pub const HS_PROMOTE: usize = 16 * 1024;
#[path = "seg_map_weight.rs"]
mod weight;
pub(crate) use weight::arc_box;

/// Local-depth ceiling — a pathological key population (adversarial
/// top-bit collisions survive fmix64 only in theory) stops splitting
/// here and lets the one bucket grow flat instead of looping.
const MAX_BITS: u8 = 40;

/// The top bit of a cursor [`SegMap::scan_page`] hands out; a flat map's
/// cursor never has it.
pub const SEG_CURSOR: u64 = 1 << 63;

#[derive(Debug)]
pub(crate) struct Bucket<V> {
    local_bits: u8,
    map: KevyMap<SmallBytes, V>,
}

impl<V: Clone> Clone for Bucket<V> {
    fn clone(&self) -> Self {
        Bucket { local_bits: self.local_bits, map: self.map.clone() }
    }
}

/// A giant hash/set: extendible-hash directory over `Arc`-shared
/// buckets. `V = SmallBytes` is the hash door, `V = ()` the set door.
///
/// ```
/// use kevy_store::SmallBytes;
/// use kevy_store::seg_map::SegMap;
/// let mut fields: SegMap<SmallBytes> = SegMap::default();
/// fields.insert(SmallBytes::from_slice(b"name"), SmallBytes::from_slice(b"alice"));
/// assert_eq!(fields.get(b"name").map(SmallBytes::as_slice), Some(&b"alice"[..]));
/// ```
#[derive(Debug)]
pub struct SegMap<V> {
    global_bits: u8,
    /// `1 << global_bits` entries; each names a bucket index.
    dirs: Vec<u32>,
    buckets: Vec<Arc<Bucket<V>>>,
    len: usize,
}

impl<V: Clone> Clone for SegMap<V> {
    fn clone(&self) -> Self {
        SegMap {
            global_bits: self.global_bits,
            dirs: self.dirs.clone(),
            buckets: self.buckets.clone(),
            len: self.len,
        }
    }
}

impl<V: Clone> Default for SegMap<V> {
    fn default() -> Self {
        SegMap {
            global_bits: 0,
            dirs: alloc::vec![0],
            buckets: alloc::vec![Arc::new(Bucket { local_bits: 0, map: KevyMap::new() })],
            len: 0,
        }
    }
}

impl<V: Clone> SegMap<V> {
    #[inline]
    /// Elements across every bucket. Kept as a running count rather than
    /// summed on call, so this is O(1) at any directory size.
    ///
    /// ```
    /// use kevy_store::SmallBytes;
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<()> = SegMap::default();
    /// m.insert(SmallBytes::from_slice(b"a"), ());
    /// m.insert(SmallBytes::from_slice(b"a"), ());
    /// assert_eq!(m.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    /// Whether the map holds nothing. Note this is about elements, not
    /// buckets — an emptied map keeps its directory.
    ///
    /// ```
    /// use kevy_store::SmallBytes;
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<()> = SegMap::default();
    /// assert!(m.is_empty());
    /// m.insert(SmallBytes::from_slice(b"a"), ());
    /// m.remove(b"a");
    /// assert!(m.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One page of a `*SCAN` sweep: whole buckets, in the order of the
    /// hash prefixes they cover, until `count` entries are out. The
    /// cursor is the next prefix to visit (its top bit set, its lowest bit
    /// dropped), so a bucket splitting or the directory doubling between
    /// pages skips nothing below it; a cursor that is not one of these
    /// starts the sweep over. `0` ends the sweep.
    ///
    /// ```
    /// use kevy_store::SmallBytes;
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<()> = SegMap::default();
    /// for i in 0..5_000u32 {
    ///     m.insert(SmallBytes::from_slice(&i.to_be_bytes()), ());
    /// }
    /// let (mut seen, mut cur) = (Vec::new(), 0u64);
    /// loop {
    ///     cur = m.scan_page(cur, 100, |k, _| seen.push(k.to_vec()));
    ///     if cur == 0 { break; }
    /// }
    /// seen.sort_unstable();
    /// seen.dedup();
    /// assert_eq!(seen.len(), 5_000);
    /// ```
    pub fn scan_page(&self, cursor: u64, count: usize, mut f: impl FnMut(&[u8], &V)) -> u64 {
        let mut prefix = if cursor & SEG_CURSOR != 0 { (cursor & !SEG_CURSOR) << 1 } else { 0 };
        let mut emitted = 0;
        loop {
            let b = &self.buckets[self.dirs[self.route(prefix)] as usize];
            for (k, v) in b.map.iter() {
                f(k.as_slice(), v);
            }
            emitted += b.map.len();
            // the bucket covers every prefix sharing its top `local_bits`
            let span = 1u64.checked_shl(64 - u32::from(b.local_bits));
            let Some(next) = span.and_then(|s| (prefix & !(s - 1)).checked_add(s)) else {
                return 0;
            };
            prefix = next;
            if emitted >= count {
                return SEG_CURSOR | (prefix >> 1);
            }
        }
    }

    /// Directory slot for a hash (top `global_bits` bits).
    #[inline]
    fn route(&self, hash: u64) -> usize {
        if self.global_bits == 0 { 0 } else { (hash >> (64 - self.global_bits)) as usize }
    }

    #[inline]
    fn bucket_of(&self, key: &[u8]) -> usize {
        self.dirs[self.route(key.kevy_hash())] as usize
    }

    /// Look one key up: hash to a directory slot, then probe that bucket.
    /// Two indirections regardless of how large the map has grown, and no
    /// clone — reads never trigger the copy-on-write path.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// m.insert(b"a"[..].into(), 1);
    /// assert_eq!(m.get(b"a"), Some(&1));
    /// assert_eq!(m.get(b"b"), None);
    /// ```
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        self.buckets[self.bucket_of(key)].map.get(key)
    }

    /// Membership, on the same one-bucket path as `get` and without
    /// materialising the value.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<()> = SegMap::default();
    /// m.insert(b"a"[..].into(), ());
    /// assert!(m.contains_key(b"a"));
    /// assert!(!m.contains_key(b"b"));
    /// ```
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.buckets[self.bucket_of(key)].map.contains_key(key)
    }

    /// Insert; COW cost is the routed bucket (plus a bounded split).
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// assert_eq!(m.insert(b"a"[..].into(), 1), None);
    /// assert_eq!(m.insert(b"a"[..].into(), 2), Some(1));
    /// ```
    pub fn insert(&mut self, key: SmallBytes, value: V) -> Option<V> {
        let slot = self.route(key.as_slice().kevy_hash());
        let bi = self.dirs[slot] as usize;
        let b = Arc::make_mut(&mut self.buckets[bi]);
        let old = b.map.insert(key, value);
        if old.is_none() {
            self.len += 1;
            if b.map.len() > BUCKET_SPLIT {
                self.split(slot);
            }
        }
        old
    }

    /// Remove one key, returning what it held. This is a write: while a
    /// snapshot pins the structure it clones the outer shell plus the one
    /// bucket it routes to, never the whole map — the bound the module
    /// exists for.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// m.insert(b"a"[..].into(), 1);
    /// assert_eq!(m.remove(b"a"), Some(1));
    /// assert_eq!(m.remove(b"a"), None);
    /// ```
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        let bi = self.bucket_of(key);
        let old = Arc::make_mut(&mut self.buckets[bi]).map.remove(key);
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    /// Split the bucket routed from directory slot `slot` (over
    /// threshold). Doubles the directory first when the bucket is at
    /// global depth; repeats while a half is still oversized (bounded
    /// by [`MAX_BITS`]).
    fn split(&mut self, mut slot: usize) {
        loop {
            let bi = self.dirs[slot] as usize;
            let lb = self.buckets[bi].local_bits;
            if self.buckets[bi].map.len() <= BUCKET_SPLIT || lb >= MAX_BITS {
                return;
            }
            if lb == self.global_bits {
                self.double_directory();
                slot <<= 1;
            }
            let k = self.global_bits - lb;
            let span = 1usize << k;
            let start = (slot >> k) << k;
            let (lo, hi) = self.partition_bucket(bi, lb);
            self.buckets[bi] = Arc::new(lo);
            let hi_ix = self.buckets.len() as u32;
            self.buckets.push(Arc::new(hi));
            for s in start + span / 2..start + span {
                self.dirs[s] = hi_ix;
            }
            // Continue in whichever half `slot` now routes to; the
            // loop head re-checks its size.
        }
    }

    /// Double the directory: index copies only, buckets untouched.
    fn double_directory(&mut self) {
        let mut next = Vec::with_capacity(self.dirs.len() * 2);
        for &d in &self.dirs {
            next.push(d);
            next.push(d);
        }
        self.dirs = next;
        self.global_bits += 1;
    }

    /// Rebuild bucket `bi` (local depth `lb`) as two buckets of depth
    /// `lb + 1`, partitioned by the next hash bit from the top.
    fn partition_bucket(&self, bi: usize, lb: u8) -> (Bucket<V>, Bucket<V>) {
        let src = &self.buckets[bi].map;
        let mut lo = KevyMap::with_capacity(src.len() / 2);
        let mut hi = KevyMap::with_capacity(src.len() / 2);
        let bit = 1u64 << (63 - lb);
        for (k, v) in src.iter() {
            if k.as_slice().kevy_hash() & bit == 0 {
                lo.insert(k.clone(), v.clone());
            } else {
                hi.insert(k.clone(), v.clone());
            }
        }
        (Bucket { local_bits: lb + 1, map: lo }, Bucket { local_bits: lb + 1, map: hi })
    }

    /// Every entry, bucket by bucket. Order follows the directory rather
    /// than insertion, and is not stable across a split.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// m.insert(b"a"[..].into(), 1);
    /// m.insert(b"b"[..].into(), 2);
    /// assert_eq!(m.iter().map(|(_, v)| v).sum::<u32>(), 3);
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = (&SmallBytes, &V)> {
        self.buckets.iter().flat_map(|b| b.map.iter())
    }

    /// Keys only, in `iter`'s order.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// m.insert(b"a"[..].into(), 1);
    /// m.insert(b"b"[..].into(), 2);
    /// let mut keys: Vec<&[u8]> = m.keys().map(|k| k.as_slice()).collect();
    /// keys.sort();
    /// assert_eq!(keys, [&b"a"[..], b"b"]);
    /// ```
    pub fn keys(&self) -> impl Iterator<Item = &SmallBytes> {
        self.iter().map(|(k, _)| k)
    }

    /// Values only, in `iter`'s order.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// m.insert(b"a"[..].into(), 1);
    /// m.insert(b"b"[..].into(), 2);
    /// let mut values: Vec<u32> = m.values().copied().collect();
    /// values.sort();
    /// assert_eq!(values, [1, 2]);
    /// ```
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// An arbitrary member, seeded by `draw`: bucket weighted by length
    /// (prefix walk), then the bucket's own random-slot probe. Same
    /// "arbitrary, slightly biased" contract as the flat
    /// `iter_from_slot` path.
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// let mut m: SegMap<u32> = SegMap::default();
    /// assert!(m.rand_entry(42).is_none());
    /// m.insert(b"a"[..].into(), 1);
    /// m.insert(b"b"[..].into(), 2);
    /// let (k, v) = m.rand_entry(42).unwrap();
    /// assert_eq!(m.get(k.as_slice()), Some(v));
    /// ```
    pub fn rand_entry(&self, draw: u64) -> Option<(&SmallBytes, &V)> {
        if self.len == 0 {
            return None;
        }
        let mut target = crate::rng::below(draw, self.len as u64) as usize;
        for b in &self.buckets {
            let m = &b.map;
            if target < m.len() {
                // iter_from_bucket walks to the END only (no wrap) —
                // fall back to a front sweep so a draw landing past the
                // last occupied slot still yields a member.
                let s = (draw as usize) % m.capacity().max(1);
                return m.iter_from_bucket(s).next().or_else(|| m.iter().next());
            }
            target -= m.len();
        }
        None
    }

    /// Build from a flat map through the routing insert (splits engage
    /// as buckets fill).
    ///
    /// ```
    /// use kevy_store::seg_map::SegMap;
    /// use kevy_store::{HashData, SmallBytes};
    /// let mut flat = HashData::default();
    /// flat.insert(SmallBytes::from_slice(b"f"), SmallBytes::from_slice(b"v"));
    /// let m = SegMap::from_flat(flat);
    /// assert_eq!(m.get(b"f").map(SmallBytes::as_slice), Some(&b"v"[..]));
    /// ```
    pub fn from_flat(flat: KevyMap<SmallBytes, V>) -> Self {
        let mut out = SegMap::default();
        for (k, v) in flat.iter() {
            out.insert(k.clone(), v.clone());
        }
        out
    }

    /// Every bucket unique — the bio-drop gate (a view-shared drop is
    /// refcount decrements, cheap inline).
    pub(crate) fn all_unique(&self) -> bool {
        self.buckets.iter().all(|b| Arc::strong_count(b) == 1)
    }

    /// Test-only bucket introspection: `(strong_count, len)` per bucket
    /// — COW tests assert which buckets a write actually cloned.
    #[cfg(test)]
    pub(crate) fn bucket_stats(&self) -> Vec<(usize, usize)> {
        self.buckets.iter().map(|b| (Arc::strong_count(b), b.map.len())).collect()
    }
}

impl SegMap<()> {
    /// [`crate::Value::weight`]'s SegSet arm — the flat Set model plus
    /// the shell.
    pub(crate) fn weight_as_set(&self) -> u64 {
        crate::seg_map::arc_box::<Self>()
            + self.shell_bytes()
            + self.keys().map(crate::hash_weight::held).sum::<u64>()
    }
}
