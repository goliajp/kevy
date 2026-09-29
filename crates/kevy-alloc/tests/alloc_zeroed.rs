//! Zeroed allocations under the installed allocator.
//!
//! `vec![0u8; n]` asks for `alloc_zeroed`. `GlobalAlloc`'s default answers
//! it with `alloc` followed by a memset, and for a block past the size
//! classes that memset writes every page of a mapping the kernel had
//! already zeroed — a 64 MiB receive ring became 64 MiB resident before a
//! byte arrived.

use std::alloc::{GlobalAlloc, Layout};

#[global_allocator]
static ALLOC: kevy_alloc::KevyAlloc = kevy_alloc::KevyAlloc;

unsafe extern "C" {
    fn mincore(addr: *mut core::ffi::c_void, len: usize, vec: *mut u8) -> i32;
}

/// Pages of `[ptr, ptr + len)` the kernel reports resident.
fn resident_pages(ptr: *const u8, len: usize) -> usize {
    // mincore wants a page-aligned start; the block's own start may not be
    let page = kevy_alloc::os::PAGE;
    let start = ptr as usize & !(page - 1);
    let span = ptr as usize + len - start;
    let mut vec = vec![0u8; span.div_ceil(page)];
    // SAFETY: `vec` holds one byte per 4 KiB of the range, at least one per
    // page whatever the system page size, and the range is mapped.
    let rc = unsafe { mincore(start as *mut _, span, vec.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore failed");
    vec.iter().filter(|b| **b & 1 != 0).count()
}

#[test]
fn a_large_zeroed_block_is_not_written_into_residence() {
    if !kevy_alloc::os::available() {
        return;
    }
    let len = 64 << 20;
    let ring = vec![0u8; len];
    let resident = resident_pages(ring.as_ptr(), len);
    assert_eq!(resident, 0, "{resident} pages of a fresh zeroed mapping were written to");
    // untouched is not unreadable
    assert!(ring.iter().step_by(4096).all(|b| *b == 0));
}

/// A reused block is not fresh, so it must still come back zeroed — the
/// retention pool hands out mappings a previous owner wrote to, and the
/// size classes hand out slots.
#[test]
fn a_reused_block_comes_back_zeroed() {
    if !kevy_alloc::os::available() {
        return;
    }
    // a length nothing else in this binary asks for, so the pooled mapping
    // taken below is the one freed here
    for size in [kevy_alloc::class::MAX_SMALL + 12_345, 3_000, 40] {
        let layout = Layout::from_size_align(size, 8).expect("valid layout");
        // SAFETY: a non-zero layout; every block is freed with the layout it was made with.
        unsafe {
            let dirty = ALLOC.alloc(layout);
            assert!(!dirty.is_null());
            core::ptr::write_bytes(dirty, 0xAB, size);
            ALLOC.dealloc(dirty, layout);
            let zeroed = ALLOC.alloc_zeroed(layout);
            assert!(!zeroed.is_null());
            let bytes = core::slice::from_raw_parts(zeroed, size);
            assert!(bytes.iter().all(|b| *b == 0), "a reused {size}-byte block kept old bytes");
            ALLOC.dealloc(zeroed, layout);
        }
    }
}
