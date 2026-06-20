//! Allocation lifecycle for [`crate::KevyMap`] — `alloc_table` (the only
//! growing constructor) and the matching `Drop` impl. Split out so
//! [`crate::map`] stays under the 500-LOC house rule. Both halves dispatch
//! between the global allocator and a 2 MiB-aligned `mmap` path (E13)
//! based on the per-instance `mmap_backed` flag.

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;
use core::ptr::NonNull;
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};

use crate::map::{EMPTY, GROUP_WIDTH, KevyMap, MIN_CAP, table_layout};

/// Tables smaller than this stay on the global allocator. mmap's
/// over-allocate-and-trim alignment trick costs one extra HP of address
/// space + 2 munmap syscalls — only worth it once the payload itself
/// approaches HP scale.
const THP_BACKED_THRESHOLD: usize = 1024 * 1024; // 1 MiB

impl<K, V> KevyMap<K, V> {
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
        let meta_byte_ptr = unsafe { base.add(meta_offset) };
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
            // SAFETY: alloc returned non-null; raw pointers are derived
            // within the same allocation.
            slots_ptr: unsafe { NonNull::new_unchecked(slots_ptr) },
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
        if std::mem::needs_drop::<(K, V)>() {
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
