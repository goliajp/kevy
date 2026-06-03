//! The KevyMap implementation: struct, allocation, probing, and the live
//! lookup / insert / remove API. Helpers (`h2`, `prefetch_t0`, metadata
//! constants) and the private `ProbeOutcome` enum are all map-scoped.
//!
//! Layout (single allocation):
//!
//! ```text
//! +------------+------------+-----+---------------+---------+--------+
//! | slot[0]    | slot[1]    | ... | slot[cap-1]   | padding | meta   |
//! +------------+------------+-----+---------------+---------+--------+
//! ^                                                          ^
//! slots_ptr                                                  metadata_ptr
//! ```
//!
//! Both pointers are precomputed at `alloc_table` time and never re-derived
//! in the hot path. The single allocation cuts one alloc/dealloc pair vs
//! the previous two-`Box<[…]>` layout, and keeps metadata + slots in
//! adjacent pages (warmer TLB, contiguous OS-prefetch).

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use kevy_hash::KevyHash;

use crate::iter::{Iter, Keys, Values};

/// SIMD group width (16 metadata bytes loaded per probe iteration).
pub(crate) const GROUP_WIDTH: usize = 16;

/// Metadata byte for an empty slot (top bit set, value bits 1's — distinct
/// from DELETED so the probe loop can stop at EMPTY but skip DELETED).
pub(crate) const EMPTY: u8 = 0xFF;
/// Metadata byte for a tombstone (top bit set, value bits 0).
pub(crate) const DELETED: u8 = 0x80;
/// Minimum table size. ≥ 16 (one SSE2 group) so the future SIMD path can run
/// a full group scan unconditionally.
pub(crate) const MIN_CAP: usize = 16;

/// Top-7 bits of the hash, used as the per-slot metadata byte for occupied
/// slots. The top bit is always 0 (so occupancy = `meta & 0x80 == 0`).
#[inline]
pub(crate) fn h2(hash: u64) -> u8 {
    ((hash >> 57) & 0x7F) as u8
}

/// Issue a hint to fetch the cache line containing `ptr` into L1 ("T0" =
/// "all levels"). Stable on x86_64 / aarch64; no-op elsewhere AND under
/// `cfg(miri)` (miri cannot model inline asm / arch intrinsics, so the hint
/// degrades to a no-op for unsafe-correctness testing — the semantic
/// contract of `prefetch_t0` is "may do nothing", so this is sound).
#[inline(always)]
fn prefetch_t0(ptr: *const u8) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
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
    #[cfg(all(target_arch = "aarch64", not(miri)))]
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
    #[cfg(any(miri, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
    {
        let _ = ptr;
    }
}

/// Compute the single-buffer layout for a table of `cap` slots: returns the
/// combined `Layout` and the byte offset to the metadata array. Panics on
/// arithmetic overflow (only reachable for cap ≈ usize::MAX which would OOM
/// anyway).
///
/// Metadata size is `cap + GROUP_WIDTH` (hashbrown 0.15 layout): the first
/// `cap` bytes are the real per-slot metadata, the trailing `GROUP_WIDTH`
/// bytes mirror the leading ones so the branchless `set_meta` formula
/// `index2 = ((i - GW) & mask) + GW` always lands inside the buffer (for
/// `i = GROUP_WIDTH - 1` the formula evaluates to `cap + GROUP_WIDTH - 1`,
/// the very last byte). That last byte is written by `set_meta` but never
/// read by `Group::load` — SIMD loads from `group_start ∈ [0, cap)` reach
/// at most `cap + GROUP_WIDTH - 2`.
#[inline]
pub(crate) fn table_layout<KV>(cap: usize) -> (Layout, usize) {
    let slots = Layout::array::<MaybeUninit<KV>>(cap).expect("slots layout overflow");
    let meta = Layout::array::<u8>(cap + GROUP_WIDTH).expect("metadata layout overflow");
    let (combined, meta_offset) = slots.extend(meta).expect("layout extend overflow");
    (combined.pad_to_align(), meta_offset)
}

/// An open-addressing Swiss-style hashtable keyed by [`KevyHash`].
///
/// Power-of-two capacity (`mask = cap - 1`); 7/8 load factor; linear probing
/// over the metadata array; full slots' (K, V) live AoS in a parallel slot
/// array of `MaybeUninit<(K, V)>` co-allocated with the metadata.
///
/// When `cap == 0` both pointers are dangling and no allocation is held.
pub struct KevyMap<K, V> {
    /// Slot array. `cap` initialised iff the corresponding metadata byte is
    /// in `0x00..=0x7F`. Dangling when `cap == 0`.
    pub(crate) slots_ptr: NonNull<MaybeUninit<(K, V)>>,
    /// Metadata array (`cap + GROUP_WIDTH` bytes; trailing
    /// `GROUP_WIDTH - 1` bytes mirror the leading ones for SIMD-safe
    /// wraparound loads — the hashbrown layout). Dangling when `cap == 0`.
    pub(crate) metadata_ptr: NonNull<u8>,
    /// Allocated slot count. `0` when no allocation is held.
    pub(crate) cap: usize,
    /// `cap - 1` when `cap > 0`; `0` when `cap == 0`.
    pub(crate) mask: usize,
    /// Live entries.
    pub(crate) occupied: usize,
    /// Tombstones (not yet reclaimed).
    pub(crate) deleted: usize,
    /// Marker so dropck and variance treat us as owning `(K, V)` like a
    /// `Box<[MaybeUninit<(K, V)>]>` would.
    _marker: PhantomData<(K, V)>,
}

// SAFETY: KevyMap owns its `(K, V)` entries (via the slot allocation). The
// `NonNull<...>` fields are conceptually `Box<[…]>` and inherit the same
// Send/Sync bounds: send-K + send-V ⇒ KevyMap is Send. Same for Sync.
unsafe impl<K: Send, V: Send> Send for KevyMap<K, V> {}
unsafe impl<K: Sync, V: Sync> Sync for KevyMap<K, V> {}

/// `(metadata, slots)` parallel-slice pair returned by [`KevyMap::as_slices`].
/// Aliased so the long `(&[u8], &[MaybeUninit<(K, V)>])` signature doesn't
/// trip clippy's `type_complexity` lint on a member-by-member basis.
type SlotSlices<'a, K, V> = (&'a [u8], &'a [MaybeUninit<(K, V)>]);

pub(crate) enum ProbeOutcome {
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
            slots_ptr: NonNull::dangling(),
            metadata_ptr: NonNull::dangling(),
            cap: 0,
            mask: 0,
            occupied: 0,
            deleted: 0,
            _marker: PhantomData,
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

    pub(crate) fn alloc_table(cap: usize) -> Self {
        debug_assert!(cap.is_power_of_two());
        debug_assert!(cap >= MIN_CAP);

        let (layout, meta_offset) = table_layout::<(K, V)>(cap);
        // SAFETY: layout has non-zero size (metadata alone is ≥ MIN_CAP +
        // GROUP_WIDTH - 1 ≥ 31 bytes). alloc returns either a valid
        // allocation of `layout` or null.
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            handle_alloc_error(layout);
        }
        // Initialise the metadata range (real + mirror tail) to EMPTY in a
        // single memset. The slot array is left uninitialised — slots
        // become initialised only when their metadata byte transitions
        // out of the high-bit-set state (EMPTY/DELETED).
        let meta_byte_ptr = unsafe { base.add(meta_offset) };
        unsafe { ptr::write_bytes(meta_byte_ptr, EMPTY, cap + GROUP_WIDTH) };

        let slots_ptr = base as *mut MaybeUninit<(K, V)>;
        let metadata_ptr = meta_byte_ptr;

        // single-buffer redo: hint THP on the entire buffer in
        // one madvise call. The combined allocation is `meta_offset +
        // cap + GROUP_WIDTH` bytes (== `layout.size()` minus padding).
        // On 10M+ key tables the metadata alone is 16 MB — well over the
        // 2 MB HP boundary, so the kernel's khugepaged can promote it in
        // place. Cheap on the non-Linux paths (compile-time no-op).
        kevy_madvise::advise_hugepage(base as *const u8, layout.size());

        Self {
            // SAFETY: alloc returned non-null; raw pointers are derived
            // within the same allocation.
            slots_ptr: unsafe { NonNull::new_unchecked(slots_ptr) },
            metadata_ptr: unsafe { NonNull::new_unchecked(metadata_ptr) },
            cap,
            mask: cap - 1,
            occupied: 0,
            deleted: 0,
            _marker: PhantomData,
        }
    }

    /// Write `v` into metadata slot `i`, also updating the mirror byte
    /// at `cap + i` when `i < GROUP_WIDTH`. Every metadata mutation goes
    /// through this helper so the mirror stays consistent with the real
    /// metadata.
    ///
    /// Branchless: the formula `index2 = ((i - GW) & mask) + GW`
    /// (hashbrown 0.15's `set_ctrl`) yields the real mirror position
    /// `cap + i` when `i < GW`, and yields `i` itself when `i >= GW`.
    /// The second write is therefore either to the mirror byte or a
    /// duplicate write to the same real byte (a no-op). No branch.
    #[inline]
    pub(crate) fn set_meta(&mut self, i: usize, v: u8) {
        debug_assert!(i < self.cap);
        // SAFETY: i ∈ [0, cap); i2 ∈ [GROUP_WIDTH, cap + GROUP_WIDTH);
        // both in-bounds since metadata buffer length is cap + GROUP_WIDTH.
        let i2 = (i.wrapping_sub(GROUP_WIDTH) & self.mask) + GROUP_WIDTH;
        unsafe {
            *self.metadata_ptr.as_ptr().add(i) = v;
            *self.metadata_ptr.as_ptr().add(i2) = v;
        }
    }

    /// Live entry count.
    #[inline]
    pub fn len(&self) -> usize {
        self.occupied
    }

    /// Whether the map has zero live entries.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    /// Allocated slot count (NOT live entries).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Drop every live entry and reset the metadata. Keeps the allocation.
    pub fn clear(&mut self) {
        if self.cap == 0 {
            return;
        }
        if std::mem::needs_drop::<(K, V)>() {
            for i in 0..self.cap {
                // SAFETY: i < cap ⇒ metadata pointer in-bounds.
                let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
                if meta & 0x80 == 0 {
                    // SAFETY: full slot ⇒ initialised.
                    unsafe {
                        ptr::drop_in_place(self.slots_ptr.as_ptr().add(i) as *mut (K, V));
                    }
                }
            }
        }
        // Reset entire metadata buffer (real range + mirror tail) in one memset.
        // SAFETY: metadata buffer is exactly cap + GROUP_WIDTH bytes wide.
        unsafe {
            ptr::write_bytes(self.metadata_ptr.as_ptr(), EMPTY, self.cap + GROUP_WIDTH);
        }
        self.occupied = 0;
        self.deleted = 0;
    }

    /// `(&K, &V)` over all live entries; order is unspecified.
    pub fn iter(&self) -> Iter<'_, K, V> {
        let (metadata, slots) = self.as_slices();
        Iter::new(metadata, slots)
    }

    /// `iter` that begins at bucket `start` (clamped to `capacity()`) and
    /// walks to the end. To sweep the full ring beginning at a random offset
    /// — the pattern the kevy-store eviction sampler uses — chain it with a
    /// second `iter_from_bucket(0)` and `take(start)`.
    pub fn iter_from_bucket(&self, start: usize) -> Iter<'_, K, V> {
        let (metadata, slots) = self.as_slices();
        Iter::with_start(metadata, slots, start)
    }

    /// `&K` over all live entries.
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys::new(self.iter())
    }

    /// `&V` over all live entries.
    pub fn values(&self) -> Values<'_, K, V> {
        Values::new(self.iter())
    }

    /// Borrow the metadata and slots as parallel slices of length `cap`.
    /// Used by [`KevyMap::iter`] (which only needs the real slot range,
    /// not the mirror tail). When `cap == 0` returns two empty slices —
    /// the dangling pointer is never dereferenced.
    #[inline]
    fn as_slices(&self) -> SlotSlices<'_, K, V> {
        if self.cap == 0 {
            return (&[], &[]);
        }
        // SAFETY: cap > 0 ⇒ both pointers are valid for `cap` reads; we hand
        // out shared borrows tied to `&self`'s lifetime, so the allocation
        // outlives the returned slices.
        unsafe {
            (
                std::slice::from_raw_parts(self.metadata_ptr.as_ptr(), self.cap),
                std::slice::from_raw_parts(self.slots_ptr.as_ptr(), self.cap),
            )
        }
    }

    /// Hint the CPU to fetch the bucket cache line that a probe at `hash`
    /// would start at. The prefetch lever against the bucket-probe DRAM
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
        if self.cap == 0 {
            return;
        }
        let idx = (hash as usize) & self.mask;
        // SAFETY: idx < cap ≤ metadata length ⇒ pointer in-bounds; prefetch
        // reads never trap and never observe values.
        let ptr = unsafe { self.metadata_ptr.as_ptr().add(idx) };
        prefetch_t0(ptr);
    }

    /// 7/8 of the capacity — the inclusive max for `occupied + deleted`.
    #[inline]
    pub(crate) fn threshold(&self) -> usize {
        self.cap - (self.cap / 8)
    }
}


impl<K, V> Default for KevyMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// `m[&q]` panics on missing key (matches `std::HashMap::Index` semantics).
impl<K, Q, V> std::ops::Index<&Q> for KevyMap<K, V>
where
    K: std::borrow::Borrow<Q>,
    Q: KevyHash + Eq + ?Sized,
{
    type Output = V;
    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("no entry found for key")
    }
}

impl<K, V> Drop for KevyMap<K, V> {
    fn drop(&mut self) {
        if self.cap == 0 {
            return;
        }
        if std::mem::needs_drop::<(K, V)>() {
            for i in 0..self.cap {
                // SAFETY: i < cap ⇒ in-bounds.
                let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
                if meta & 0x80 == 0 {
                    // SAFETY: full slot ⇒ initialised.
                    unsafe {
                        ptr::drop_in_place(self.slots_ptr.as_ptr().add(i) as *mut (K, V));
                    }
                }
            }
        }
        // Free the single combined allocation. `slots_ptr` IS the base of
        // the allocation (see alloc_table's layout computation: slots are
        // at offset 0; metadata sits at meta_offset).
        let (layout, _) = table_layout::<(K, V)>(self.cap);
        // SAFETY: cap > 0 ⇒ slots_ptr is non-null and was returned by `alloc`
        // with the same Layout (table_layout is deterministic on cap).
        unsafe {
            dealloc(self.slots_ptr.as_ptr() as *mut u8, layout);
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for KevyMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
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

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
