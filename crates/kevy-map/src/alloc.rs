//! Allocation lifecycle for [`crate::KevyMap`] — `alloc_table` (the only
//! growing constructor) and the matching `Drop` impl. Split out so
//! [`crate::map`] stays under the 500-LOC house rule. Both halves dispatch
//! between the global allocator and a 2 MiB-aligned `mmap` path (E13)
//! based on the per-instance `mmap_backed` flag.

use alloc_crate::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;
use core::ptr::NonNull;

use crate::map::{EMPTY, GROUP_WIDTH, KevyMap, MIN_CAP, table_layout};

/// Tables smaller than this stay on the global allocator. mmap's
/// over-allocate-and-trim alignment trick costs one extra HP of address
/// space + 2 munmap syscalls — only worth it once the payload itself
/// approaches HP scale.
const THP_BACKED_THRESHOLD: usize = 1024 * 1024; // 1 MiB

impl<K, V> KevyMap<K, V> {
    /// The table's own allocation as the global allocator made it: its
    /// start and layout. `None` when there is no table, or when a large one
    /// was mapped directly rather than allocated. What a defrag pass asks
    /// the allocator about before copying a map.
    ///
    /// ```
    /// use kevy_map::KevyMap;
    /// let mut m: KevyMap<u32, u32> = KevyMap::new();
    /// assert!(m.table_allocation().is_none(), "nothing allocated yet");
    /// m.insert(1, 2);
    /// let (ptr, layout) = m.table_allocation().ok_or("a table")?;
    /// assert!(!ptr.is_null() && layout.size() > 0);
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn table_allocation(&self) -> Option<(*const u8, Layout)> {
        (self.cap != 0 && !self.mmap_backed).then(|| {
            (self.slots_ptr.as_ptr().cast::<u8>().cast_const(), table_layout::<(K, V)>(self.cap).0)
        })
    }

    /// Allocate a freshly-zeroed table sized for `cap` slots. `cap` must be
    /// a power of two and ≥ `MIN_CAP`. Used by [`crate::KevyMap::with_capacity`]
    /// and by the growth path on rehash.
    ///
    /// E13: when the combined layout is ≥ [`THP_BACKED_THRESHOLD`], allocate
    /// directly via 2 MiB-aligned `mmap` so the kernel's `khugepaged` can
    /// actually promote the region to 2 MiB pages. With the global
    /// allocator (jemalloc-like chunk placement) the base pointer is only
    /// 4 KiB-aligned, so `khugepaged` cannot find a candidate even when
    /// `MADV_HUGEPAGE` is set — observed as `AnonHugePages: 0 kB` in
    /// `/proc/PID/smaps` despite the hint.
    pub(crate) fn alloc_table(cap: usize) -> Self {
        debug_assert!(cap.is_power_of_two());
        debug_assert!(cap >= MIN_CAP);

        let (layout, meta_offset) = table_layout::<(K, V)>(cap);
        let (base, mmap_backed) = if layout.size() >= THP_BACKED_THRESHOLD {
            if let Some(p) = kevy_madvise::mmap_anon_aligned_2mb(layout.size()) {
                (p.as_ptr(), true)
            } else {
                (fallback_alloc(layout), false)
            }
        } else {
            (fallback_alloc(layout), false)
        };
        // Initialise the metadata range (real + mirror tail) to EMPTY in a
        // single memset. The slot array is left uninitialised — slots
        // become initialised only when their metadata byte transitions
        // out of the high-bit-set state (EMPTY/DELETED).
        // SAFETY: `meta_offset` is inside the layout `base` was allocated with.
        let meta_byte_ptr = unsafe { base.add(meta_offset) };
        // SAFETY: the layout reserved exactly `cap + GROUP_WIDTH` bytes here.
        unsafe { ptr::write_bytes(meta_byte_ptr, EMPTY, cap + GROUP_WIDTH) };

        let slots_ptr = base.cast::<MaybeUninit<(K, V)>>();
        let metadata_ptr = meta_byte_ptr;

        // Re-hint THP on the buffer. On the `mmap_backed = true` path the
        // 2 MiB-aligned mmap already called MADV_HUGEPAGE inside
        // `mmap_anon_aligned_2mb`; this second call is redundant there but
        // harmless. On the global-alloc fallback path it's the only hint.
        if !mmap_backed {
            kevy_madvise::advise_hugepage(base.cast_const(), layout.size());
        }

        Self {
            // SAFETY: the allocation is non-null (checked; the fallback aborts).
            slots_ptr: unsafe { NonNull::new_unchecked(slots_ptr) },
            // SAFETY: `base` plus a within-layout offset, so non-null for the same reason.
            metadata_ptr: unsafe { NonNull::new_unchecked(metadata_ptr) },
            cap,
            mask: cap - 1,
            occupied: 0,
            deleted: 0,
            mmap_backed,
            _marker: PhantomData,
        }
    }
}

/// Bytes a block of `size` requested bytes occupies in glibc's `malloc`:
/// a chunk of 16-byte granules that carries an 8-byte size header, 32
/// bytes at the least. 0 for 0 — nothing is allocated.
///
/// The allocator a Linux deployment runs on by default, and the model
/// memory accounting built on this crate counts with, so a charge and
/// what the process holds can be compared byte for byte.
///
/// ```
/// use kevy_map::malloc_footprint;
/// assert_eq!(malloc_footprint(0), 0);
/// assert_eq!(malloc_footprint(1), 32, "the smallest chunk");
/// assert_eq!(malloc_footprint(72), 80, "the request plus a header, in 16-byte steps");
/// assert_eq!(malloc_footprint(900), 912);
/// ```
#[inline]
#[must_use]
pub fn malloc_footprint(size: usize) -> usize {
    // header plus request, rounded up to a granule, as masks rather than a
    // division: this runs on every hash write that is accounted
    let chunk = ((size + 8 + 15) & !15).max(32);
    if size == 0 { 0 } else { chunk }
}

impl<K, V> KevyMap<K, V> {
    /// Bytes the table occupies where it lives: the whole mapping for a
    /// table large enough to be mapped directly, otherwise its one block's
    /// [`malloc_footprint`]. 0 before anything was inserted.
    ///
    /// A pure function of the capacity, so a caller can charge a growth
    /// by reading this before and after an insert.
    ///
    /// ```
    /// let mut m: kevy_map::KevyMap<u64, u64> = kevy_map::KevyMap::new();
    /// assert_eq!(m.footprint(), 0, "no table yet");
    /// m.insert(1, 1);
    /// // sixteen 16-byte slots, then one control byte each plus a trailing group
    /// assert_eq!(m.footprint(), kevy_map::malloc_footprint(16 * 16 + 16 + 16));
    /// ```
    #[inline]
    pub fn footprint(&self) -> usize {
        if self.cap == 0 {
            return 0;
        }
        // `table_layout`'s size without its overflow checks, which the
        // table passed when it was allocated: a slot and a control byte
        // per bucket, the trailing control group, padded to the slot's
        // alignment (a no-op for every power-of-two capacity >= 16)
        let kv = core::mem::size_of::<(K, V)>();
        let size =
            (self.cap * (kv + 1) + GROUP_WIDTH).next_multiple_of(core::mem::align_of::<(K, V)>());
        debug_assert_eq!(size, table_layout::<(K, V)>(self.cap).0.size());
        if self.mmap_backed { size.next_multiple_of(HUGE_PAGE) } else { malloc_footprint(size) }
    }

    /// The bytes [`Self::footprint`] will read once the table has grown to
    /// twice its capacity (sixteen slots for a map with no table yet): what
    /// a caller holding memory to a budget sets aside before the growth
    /// arrives, so the growth does not land on memory it has no room for.
    ///
    /// ```
    /// let mut m: kevy_map::KevyMap<u64, u64> = kevy_map::KevyMap::new();
    /// m.insert(1, 1);
    /// let promised = m.grown_footprint();
    /// while m.room() > 0 {
    ///     m.insert(m.len() as u64 + 1, 0);
    /// }
    /// m.insert(u64::MAX, 0); // no room left: this insert grows the table
    /// assert_eq!(m.footprint(), promised);
    /// ```
    pub fn grown_footprint(&self) -> usize {
        let cap = if self.cap == 0 { MIN_CAP } else { self.cap * 2 };
        let size = table_layout::<(K, V)>(cap).0.size();
        // `alloc_table` maps a table this large directly wherever it can
        let mapped = size >= THP_BACKED_THRESHOLD && cfg!(target_os = "linux") && !cfg!(miri);
        if mapped { size.next_multiple_of(HUGE_PAGE) } else { malloc_footprint(size) }
    }

    /// New keys the table takes before an insert grows it: 0 when the next
    /// insert rebuilds it (or there is no table yet).
    ///
    /// ```
    /// let mut m: kevy_map::KevyMap<u64, u64> = kevy_map::KevyMap::new();
    /// assert_eq!(m.room(), 0, "no table yet");
    /// m.insert(1, 1);
    /// // sixteen slots hold fourteen keys at the 7/8 load bound
    /// assert_eq!(m.room(), 14 - 1);
    /// ```
    #[inline]
    pub fn room(&self) -> usize {
        if self.cap == 0 {
            0
        } else {
            self.threshold().saturating_sub(self.occupied + self.deleted)
        }
    }

    /// Whether the next insert rebuilds the table before it probes — an
    /// overwrite included, since the check runs before the key is looked up.
    #[inline]
    pub(crate) fn grows_on_insert(&self) -> bool {
        self.cap == 0 || (self.occupied + self.deleted) >= self.threshold()
    }
}

impl<K: kevy_hash::KevyHash + Eq, V> KevyMap<K, V> {
    /// [`KevyMap::insert`], also answering by how many bytes
    /// [`Self::footprint`] moved — nonzero only when the insert had to
    /// grow the table first, and then read around the growth alone, so an
    /// insert that does not grow costs what a plain one does.
    ///
    /// ```
    /// let mut m: kevy_map::KevyMap<u32, u32> = kevy_map::KevyMap::new();
    /// let (old, grown) = m.insert_sized(1, 10);
    /// assert_eq!((old, grown), (None, m.footprint() as isize), "the first insert allocates");
    /// assert_eq!(m.insert_sized(1, 11), (Some(10), 0), "an overwrite with room moves nothing");
    /// ```
    // a wrapper: outlined, it adds a call and returns its pair through memory
    #[allow(clippy::inline_always)]
    #[inline(always)]
    pub fn insert_sized(&mut self, key: K, value: V) -> (Option<V>, isize) {
        if self.grows_on_insert() {
            let before = self.footprint();
            self.grow();
            let grown = self.footprint() as isize - before as isize;
            return (self.insert_with_room(key, value), grown);
        }
        (self.insert_with_room(key, value), 0)
    }
}

/// A directly mapped table is rounded to whole huge pages and keeps them
/// all (`kevy_madvise::mmap_anon_aligned_2mb` trims only the alignment slack).
const HUGE_PAGE: usize = 2 * 1024 * 1024;

/// Global-allocator path that aborts on OOM. Cohesive helper so both
/// branches of `alloc_table` can call it.
fn fallback_alloc(layout: Layout) -> *mut u8 {
    // SAFETY: layout has non-zero size (metadata alone is ≥ MIN_CAP +
    // GROUP_WIDTH - 1 ≥ 31 bytes). alloc returns either a valid
    // allocation of `layout` or null.
    let p = unsafe { alloc(layout) };
    if p.is_null() {
        handle_alloc_error(layout);
    }
    p
}

impl<K, V> Drop for KevyMap<K, V> {
    fn drop(&mut self) {
        if self.cap == 0 {
            return;
        }
        if core::mem::needs_drop::<(K, V)>() {
            for i in 0..self.cap {
                // SAFETY: i < cap ⇒ in-bounds.
                let meta = unsafe { *self.metadata_ptr.as_ptr().add(i) };
                if meta & 0x80 == 0 {
                    // SAFETY: full slot ⇒ initialised.
                    unsafe {
                        ptr::drop_in_place(self.slots_ptr.as_ptr().add(i).cast::<(K, V)>());
                    }
                }
            }
        }
        // Free the single combined allocation. `slots_ptr` IS the base of
        // the allocation (see `alloc_table`'s layout computation: slots are
        // at offset 0; metadata sits at meta_offset).
        let (layout, _) = table_layout::<(K, V)>(self.cap);
        // SAFETY: cap > 0 ⇒ slots_ptr is non-null and was returned by either
        // `alloc` or `mmap_anon_aligned_2mb` with the same `layout.size()`;
        // `mmap_backed` records which path was used so dealloc matches.
        if self.mmap_backed {
            // SAFETY: slots_ptr came from mmap_anon_aligned_2mb with
            // layout.size(); munmap_2mb rounds the len back up internally.
            unsafe {
                kevy_madvise::munmap_2mb(self.slots_ptr.cast(), layout.size());
            }
        } else {
            // SAFETY: slots_ptr came from `alloc` with this layout.
            unsafe {
                dealloc(self.slots_ptr.as_ptr().cast::<u8>(), layout);
            }
        }
    }
}
