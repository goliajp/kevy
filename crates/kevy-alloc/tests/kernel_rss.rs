//! The kernel's own resident count, read from `/proc/self/statm`, is a
//! process-wide number: it is only this heap's while nothing else in the
//! process allocates. So these checks live in a test binary of their own
//! and run one after the other from a single test.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic)]

use kevy_alloc::{Heap, class, os};

fn sweep_out(heap: &mut Heap) {
    for _ in 0..=kevy_alloc::PURGE_DELAY {
        heap.reclaim();
    }
}

/// Resident pages, from `/proc/self/statm` field 2 (in pages).
fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").expect("procfs");
    let pages: u64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * os::PAGE as u64
}

/// M4 in its real form: the kernel's own resident count must fall.
///
/// The model-level tests in the crate check our prediction; this checks the
/// thing the prediction is about. Linux only, and deliberately so —
/// `MADV_DONTNEED` drops pages outright, while macOS's `MADV_FREE` only
/// marks them reclaimable, so a passing assertion there would mean
/// nothing. glibc's brk arena cannot pass this at any page count, which
/// is the whole reason this crate exists.
fn m4_the_kernel_agrees_that_pages_came_back() {
    let mut heap = Heap::new(0);
    let size = 64;
    let per_span = class::slots_per_span(class::index_of(size, 8).unwrap());
    // Enough spans that the returned bytes clear ordinary process noise.
    let count = per_span * 200;
    let mut given = Vec::with_capacity(count);
    for _ in 0..count {
        let p = heap.alloc(size, 8).expect("filling spans");
        // Touch it: untouched pages are not resident, and a test that
        // never made them resident could not observe them leaving.
        // SAFETY: a live slot of at least `size` bytes.
        unsafe { core::ptr::write_bytes(p.as_ptr(), 0x5A, size) };
        given.push(p);
    }
    let peak = rss_bytes();
    for p in given {
        // SAFETY: ours, this size and alignment.
        unsafe { heap.dealloc(p, size, 8) };
    }
    sweep_out(&mut heap);
    let after = rss_bytes();
    let touched = (count * size) as u64;
    assert!(
        after + touched / 2 < peak,
        "RSS barely moved: {peak} -> {after} after freeing {touched} bytes across {} spans",
        count / per_span
    );
}

/// The kernel's own verdict on v2, Linux only (macOS MADV_FREE gives no
/// prompt guarantee — same reasoning as the whole-span M4 test).
fn v2_the_kernel_reclaims_pages_from_spans_with_survivors() {
    let mut heap = Heap::new(0);
    let size = 400;
    let c = class::index_of(size, 8).unwrap();
    let slot = class::size_of(c);
    let per_span = class::slots_per_span(c);
    let spans = 200;
    let mut given = Vec::with_capacity(per_span * spans);
    for _ in 0..per_span * spans {
        let p = heap.alloc(size, 8).expect("fill");
        // Touch, so the pages are resident and their leaving is visible.
        // SAFETY: live slot of at least `size` bytes.
        unsafe { core::ptr::write_bytes(p.as_ptr(), 0x5A, size) };
        given.push(p);
    }
    let peak = rss_bytes();
    // Free all but each span's last-page slots — every span keeps
    // survivors, so the v1 whole-span rule would return NOTHING here.
    let last_page_start = (kevy_alloc::pagemap::PAGES_PER_SPAN - 1) * os::PAGE;
    let mut survivors = Vec::new();
    for (n, p) in given.into_iter().enumerate() {
        let in_span = n % per_span;
        if (in_span + 1) * slot > last_page_start {
            survivors.push(p);
        } else {
            // SAFETY: ours, this size and alignment.
            unsafe { heap.dealloc(p, size, 8) };
        }
    }
    sweep_out(&mut heap);
    let after = rss_bytes();
    let st = heap.snapshot();
    assert!(st.balanced(), "{st:?}");
    assert!(
        after + st.returned / 2 < peak,
        "kernel RSS barely moved with survivors pinning every span: {peak} -> {after} (returned={})",
        st.returned
    );
    for p in survivors {
        // SAFETY: ours.
        unsafe { heap.dealloc(p, size, 8) };
    }
}

#[test]
fn the_kernel_sees_freed_pages_leave() {
    if !os::available() {
        eprintln!("skipped: no anonymous mapping on this target");
        return;
    }
    m4_the_kernel_agrees_that_pages_came_back();
    v2_the_kernel_reclaims_pages_from_spans_with_survivors();
}
