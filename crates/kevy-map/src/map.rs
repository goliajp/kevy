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
use std::borrow::Borrow;
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use kevy_hash::KevyHash;

use crate::group::Group;
use crate::iter::{Iter, Keys, Values};

/// SIMD group width (16 metadata bytes loaded per probe iteration).
const GROUP_WIDTH: usize = 16;

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
fn table_layout<KV>(cap: usize) -> (Layout, usize) {
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
    slots_ptr: NonNull<MaybeUninit<(K, V)>>,
    /// Metadata array (`cap + GROUP_WIDTH` bytes; trailing
    /// `GROUP_WIDTH - 1` bytes mirror the leading ones for SIMD-safe
    /// wraparound loads — the hashbrown layout). Dangling when `cap == 0`.
    metadata_ptr: NonNull<u8>,
    /// Allocated slot count. `0` when no allocation is held.
    cap: usize,
    /// `cap - 1` when `cap > 0`; `0` when `cap == 0`.
    mask: usize,
    /// Live entries.
    occupied: usize,
    /// Tombstones (not yet reclaimed).
    deleted: usize,
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

    fn alloc_table(cap: usize) -> Self {
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
    fn set_meta(&mut self, i: usize, v: u8) {
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
    fn threshold(&self) -> usize {
        self.cap - (self.cap / 8)
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
                    let kv: *mut (K, V) = self.slots_ptr.as_ptr().add(idx) as *mut (K, V);
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
                self.set_meta(insert_at, h2(hash));
                // SAFETY: insert_at < cap ⇒ slot pointer in-bounds; we write
                // (K, V) into a previously uninitialised slot.
                unsafe {
                    (*self.slots_ptr.as_ptr().add(insert_at)).write((key, value));
                }
                self.occupied += 1;
                if via_tombstone {
                    self.deleted -= 1;
                }
                None
            }
        }
    }

    fn maybe_grow(&mut self) {
        if self.cap == 0 || (self.occupied + self.deleted) >= self.threshold() {
            self.grow();
        }
    }

    fn grow(&mut self) {
        let new_cap = if self.cap == 0 {
            MIN_CAP
        } else {
            self.cap
                .checked_mul(2)
                .expect("kevy-map: capacity doubling overflow")
        };
        let mut new_map = Self::alloc_table(new_cap);
        // Move every live entry over. After ptr::read'ing a slot we mark its
        // metadata DELETED, so any subsequent Drop (incl. panic unwind) won't
        // double-free; the old allocation will free with all-DELETED metadata.
        //
        // Only iterate the real slot range `[0, cap)`; the trailing mirror
        // bytes are bookkeeping for SIMD-load wraparound, not real slots.
        // Direct metadata writes are safe here because the old `self` table
        // is going away (we swap with new_map then drop), so a stale mirror
        // doesn't matter.
        let old_cap = self.cap;
        for i in 0..old_cap {
            // SAFETY: i < old_cap ⇒ metadata in-bounds.
            let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
            if meta & 0x80 == 0 {
                // SAFETY: full slot ⇒ initialised; we mark DELETED immediately
                // so this byte is never re-read as occupied.
                let (k, v) = unsafe { ptr::read(self.slots_ptr.as_ptr().add(i) as *const (K, V)) };
                unsafe { *self.metadata_ptr.as_ptr().add(i) = DELETED };
                let hash = k.kevy_hash();
                new_map.insert_known_unique(hash, k, v);
            }
        }
        // All occupied entries are now in new_map; the old self has no live slots.
        self.occupied = 0;
        self.deleted = 0;
        std::mem::swap(self, &mut new_map);
        // new_map (now the old self) drops; metadata is all DELETED (or EMPTY
        // for previously-empty slots) ⇒ Drop walks but touches no slots.
    }

    /// Insert under the assumption that the key isn't already present (used
    /// by `grow` to repopulate the new table). Skips the duplicate-key
    /// check. Uses a 16-slot SIMD group scan to find the first EMPTY.
    fn insert_known_unique(&mut self, hash: u64, k: K, v: V) {
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask;
        loop {
            // SAFETY: metadata is `cap + GROUP_WIDTH` bytes; group_start
            // is in `[0, cap)`; the load reads 16 bytes which lie inside the
            // buffer thanks to the mirror tail.
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                let slot = (group_start + m) & self.mask;
                self.set_meta(slot, h2v);
                // SAFETY: slot < cap.
                unsafe {
                    (*self.slots_ptr.as_ptr().add(slot)).write((k, v));
                }
                self.occupied += 1;
                return;
            }
            // Linear probing by GROUP_WIDTH (tried triangular — at our 7/8
            // load factor and group-scan-aware probe, linear wins on cache
            // locality; triangular's anti-clustering only pays off at higher
            // load factors than we run).
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }

    fn probe_with_key(&self, hash: u64, key: &K) -> ProbeOutcome {
        if self.cap == 0 {
            return ProbeOutcome::NotFound {
                insert_at: 0,
                via_tombstone: false,
            };
        }
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask;

        // Fast path: no tombstones in the table ⇒ skip DELETED tracking
        // entirely. This trims one SIMD `match_byte` (and one branch) from
        // every group iteration; insert workloads with no deletions hit
        // this path exclusively.
        if self.deleted == 0 {
            loop {
                // SAFETY: see [insert_known_unique].
                let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
                for m in g.match_byte(h2v).iter() {
                    let slot = (group_start + m) & self.mask;
                    // SAFETY: matched h2 ⇒ slot is occupied ⇒ initialised.
                    let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                    if &kv.0 == key {
                        return ProbeOutcome::Found(slot);
                    }
                }
                if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                    return ProbeOutcome::NotFound {
                        insert_at: (group_start + m) & self.mask,
                        via_tombstone: false,
                    };
                }
                group_start = (group_start + GROUP_WIDTH) & self.mask;
            }
        }

        // Slow path: tombstones exist; track the first DELETED so insert
        // can reclaim it instead of growing the tombstone count.
        let mut first_deleted: Option<usize> = None;
        loop {
            // SAFETY: see [insert_known_unique].
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            for m in g.match_byte(h2v).iter() {
                let slot = (group_start + m) & self.mask;
                // SAFETY: matched h2 ⇒ slot is occupied ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                if &kv.0 == key {
                    return ProbeOutcome::Found(slot);
                }
            }
            if first_deleted.is_none()
                && let Some(m) = g.match_byte(DELETED).lowest_set()
            {
                first_deleted = Some((group_start + m) & self.mask);
            }
            if let Some(m) = g.match_byte(EMPTY).lowest_set() {
                let probe_empty = (group_start + m) & self.mask;
                return ProbeOutcome::NotFound {
                    insert_at: first_deleted.unwrap_or(probe_empty),
                    via_tombstone: first_deleted.is_some(),
                };
            }
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
    }
}

impl<K, V> KevyMap<K, V> {
    /// Borrow the value for `key`, or `None` if absent.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        // SAFETY: find_by_borrow only returns indices into full slots.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(idx)).assume_init_ref() };
        Some(&kv.1)
    }

    /// Mutably borrow the value for `key`, or `None` if absent.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        // SAFETY: full slot.
        let kv = unsafe { (*self.slots_ptr.as_ptr().add(idx)).assume_init_mut() };
        Some(&mut kv.1)
    }

    /// Whether `key` is present in the map.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        self.find_by_borrow(key).is_some()
    }

    /// Remove `key`'s entry; returns the previous value if present.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        let idx = self.find_by_borrow(key)?;
        self.set_meta(idx, DELETED);
        self.occupied -= 1;
        self.deleted += 1;
        // SAFETY: slot was full, we just marked it DELETED so it won't be
        // read again; ptr::read moves the (K, V) out.
        let (_k, v) = unsafe { ptr::read(self.slots_ptr.as_ptr().add(idx) as *const (K, V)) };
        Some(v)
    }

    fn find_by_borrow<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: KevyHash + Eq + ?Sized,
    {
        if self.cap == 0 {
            return None;
        }
        let hash = key.kevy_hash();
        let h2v = h2(hash);
        let mut group_start = (hash as usize) & self.mask;
        loop {
            // SAFETY: see [insert_known_unique]; group_start ∈ [0, cap),
            // metadata length ≥ cap + GROUP_WIDTH.
            let g = unsafe { Group::load(self.metadata_ptr.as_ptr().add(group_start)) };
            for m in g.match_byte(h2v).iter() {
                let slot = (group_start + m) & self.mask;
                // SAFETY: matched h2 ⇒ slot occupied ⇒ initialised.
                let kv = unsafe { (*self.slots_ptr.as_ptr().add(slot)).assume_init_ref() };
                if kv.0.borrow() == key {
                    return Some(slot);
                }
            }
            // EMPTY in this group ⇒ key cannot be later in the probe.
            if !g.match_byte(EMPTY).is_empty() {
                return None;
            }
            group_start = (group_start + GROUP_WIDTH) & self.mask;
        }
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
mod tests {
    use super::*;
    use crate::set::KevySet;
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

    // ---- API-surface smoke tests (coverage padding for delegating methods) --

    #[test]
    fn map_keys_iter() {
        let mut m = KevyMap::<u64, u64>::new();
        for i in 0..5u64 {
            m.insert(i, i + 10);
        }
        let mut ks: Vec<u64> = m.keys().copied().collect();
        ks.sort();
        assert_eq!(ks, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn map_values_iter() {
        let mut m = KevyMap::<u64, u64>::new();
        for i in 0..5u64 {
            m.insert(i, i + 10);
        }
        let mut vs: Vec<u64> = m.values().copied().collect();
        vs.sort();
        assert_eq!(vs, vec![10, 11, 12, 13, 14]);
    }

    #[test]
    fn map_default_is_empty() {
        let m: KevyMap<u64, u64> = KevyMap::default();
        assert!(m.is_empty());
        assert_eq!(m.capacity(), 0);
    }

    #[test]
    fn map_from_iterator() {
        let m: KevyMap<u64, u64> = (0..10u64).map(|i| (i, i * 2)).collect();
        assert_eq!(m.len(), 10);
        assert_eq!(m.get(&5u64), Some(&10));
    }

    #[test]
    fn map_extend() {
        let mut m = KevyMap::<u64, u64>::new();
        m.extend((0..5u64).map(|i| (i, i)));
        assert_eq!(m.len(), 5);
        assert_eq!(m.get(&3u64), Some(&3));
    }

    #[test]
    fn map_index_panics_on_missing() {
        let mut m = KevyMap::<u64, u64>::new();
        m.insert(1, 10);
        assert_eq!(m[&1u64], 10);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = m[&99u64];
        }));
        assert!(r.is_err(), "Index on missing key should panic");
    }

    #[test]
    fn set_with_capacity_capacity_clear() {
        let mut s: KevySet<u64> = KevySet::with_capacity(50);
        assert!(s.capacity() >= 50);
        for i in 0..10u64 {
            s.insert(i);
        }
        assert_eq!(s.len(), 10);
        s.clear();
        assert!(s.is_empty());
        // capacity preserved
        assert!(s.capacity() >= 50);
    }

    #[test]
    fn set_as_map_smoke() {
        let mut s: KevySet<u64> = KevySet::new();
        s.insert(7);
        assert_eq!(s.as_map().len(), 1);
        assert!(s.as_map().contains_key(&7u64));
    }

    #[test]
    fn set_default_debug() {
        let s: KevySet<u64> = KevySet::default();
        assert!(s.is_empty());
        let dbg = format!("{s:?}");
        assert_eq!(dbg, "{}");
    }

    #[test]
    fn set_into_iter_ref() {
        let mut s: KevySet<u64> = KevySet::new();
        for i in 0..3u64 {
            s.insert(i);
        }
        let mut sum = 0u64;
        for k in &s {
            sum += k;
        }
        assert_eq!(sum, 3);
    }

    #[test]
    fn set_from_iterator() {
        let s: KevySet<u64> = (0..5u64).collect();
        assert_eq!(s.len(), 5);
        assert!(s.contains(&3u64));
    }

    #[test]
    fn set_extend() {
        let mut s: KevySet<u64> = KevySet::new();
        s.extend(0..5u64);
        assert_eq!(s.len(), 5);
    }
}
