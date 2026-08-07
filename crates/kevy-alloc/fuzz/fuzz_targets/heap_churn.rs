#![no_main]
//! Drive a heap with an arbitrary allocate/free/reclaim script and hold
//! it to three invariants after every step.
//!
//! The shape is the one that produced the number this crate exists for:
//! mixed sizes, freed in an order unrelated to allocation, with reclaim
//! interleaved. Under glibc that pattern is what strands pages below
//! live chunks and pins RSS at its high-water mark.
//!
//! Invariants:
//! 1. The accounting identity holds exactly — an unexplained byte is
//!    where the last fragmentation problem hid.
//! 2. Live bytes equal the sum of what is still outstanding.
//! 3. Slots do not overlap: every live allocation still reads back the
//!    marker written into it, so a slot handed out twice is caught.

use libfuzzer_sys::fuzz_target;

use kevy_alloc::{Heap, class};

/// Sizes that straddle class boundaries, the small/large cut-off, and
/// the alignment-driven class skip.
const SIZES: [usize; 16] =
    [1, 8, 16, 17, 48, 129, 400, 1023, 4096, 8192, 8193, 16384, 20000, 32768, 32769, 70000];

fuzz_target!(|data: &[u8]| {
    if !kevy_alloc::os::available() {
        return;
    }
    let mut heap = Heap::new(0);
    // The alignment is carried alongside the size: `dealloc` must be
    // given the same pair `alloc` was, because alignment can select a
    // different class and therefore a different slot size. That is
    // Rust's `Layout` contract, which the GlobalAlloc shim gets for
    // free — a hand-driven caller has to honour it deliberately.
    let mut live: Vec<(core::ptr::NonNull<u8>, usize, usize, u8)> = Vec::new();
    let mut marker: u8 = 1;

    for chunk in data.chunks(2) {
        let op = chunk[0];
        let arg = *chunk.get(1).unwrap_or(&0) as usize;
        match op % 4 {
            // Allocate, and stamp the slot so an overlap is detectable.
            0 | 1 => {
                let size = SIZES[arg % SIZES.len()];
                let align = if arg % 3 == 0 { 16 } else { 8 };
                if let Some(p) = heap.alloc(size, align) {
                    assert_eq!(
                        p.as_ptr() as usize % align,
                        0,
                        "alignment {align} not honoured for {size}"
                    );
                    // SAFETY: a live slot of at least `size` bytes.
                    unsafe { core::ptr::write_bytes(p.as_ptr(), marker, size) };
                    live.push((p, size, align, marker));
                    marker = marker.wrapping_add(1).max(1);
                }
            }
            2 => {
                if !live.is_empty() {
                    let (p, size, align, _) = live.swap_remove(arg % live.len());
                    // SAFETY: allocated here with exactly this size and
                    // alignment, and dropped from the live set now.
                    unsafe { heap.dealloc(p, size, align) };
                }
            }
            _ => heap.reclaim(),
        }

        // Small allocations are the heap's; direct mappings are counted
        // for the process (T2: a large block has no segment, so no owner
        // to route a foreign free to). Each figure balances alone, and
        // the fuzz process is single-threaded, so their sum is exact.
        let mut st = heap.snapshot();
        assert!(st.balanced(), "identity broken: {st:?}");
        st.merge(&kevy_alloc::large_stats());
        let expect: u64 = live.iter().map(|(_, s, _, _)| *s as u64).sum();
        assert_eq!(st.live, expect, "live bytes drifted");
        for (p, size, _, m) in &live {
            // SAFETY: still-live slots, stamped when handed out.
            let seen = unsafe { p.as_ptr().read() };
            assert_eq!(seen, *m, "a {size}-byte slot was handed out twice");
        }
    }

    for (p, size, align, _) in live {
        // SAFETY: still live, allocated here with this size and alignment.
        unsafe { heap.dealloc(p, size, align) };
    }
    let mut st = heap.snapshot();
    assert!(st.balanced(), "identity broken after drain: {st:?}");
    st.merge(&kevy_alloc::large_stats());
    assert_eq!(st.live, 0);
    let _ = class::MAX_SMALL;
});
