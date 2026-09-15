//! How many allocations building a tree costs, counted rather than
//! reasoned about.
//!
//! The crate itself is `#![forbid(unsafe_code)]`, so the counting global
//! allocator lives here, in a separate crate, where installing one is
//! allowed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use kevy_ranktree::RankTree;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct
// allocator; the counters are atomics and add no aliasing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) == 1 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(l.size(), Ordering::Relaxed);
        }
        // SAFETY: `l` came from the caller and is forwarded unchanged.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if COUNTING.load(Ordering::Relaxed) == 1 {
            LIVE_BYTES.fetch_sub(l.size(), Ordering::Relaxed);
        }
        // SAFETY: `p`/`l` came from the caller and are forwarded unchanged.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) == 1 {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(new, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(l.size(), Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocation calls made, and bytes still held when `f` returned.
fn count<F: FnOnce()>(f: F) -> (usize, usize) {
    ALLOCS.store(0, Ordering::Relaxed);
    LIVE_BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(1, Ordering::Relaxed);
    f();
    let held = LIVE_BYTES.load(Ordering::Relaxed);
    COUNTING.store(0, Ordering::Relaxed);
    (ALLOCS.load(Ordering::Relaxed), held)
}

/// Building a tree by insertion allocates a bounded number of times, and
/// holds a bounded number of bytes. Both are written down here so a
/// change to either is a change to a number rather than a thing nobody
/// notices.
///
/// Measured on this test, 10,000 `u64` inserts, before and after sizing
/// the node vectors to their ceiling:
///
/// |            | allocations | bytes held |
/// |------------|------------:|-----------:|
/// | growing    |       4,152 |    521,648 |
/// | reserved   |       1,393 |    291,792 |
///
/// Both fell, which was not the expected trade — reserving capacity
/// usually buys fewer allocations with more bytes. The reason is
/// `split_off`: it seeds the right half at exactly `len - mid - 1`,
/// seven keys, and doubling from seven goes 7 → 14 → 28. A node whose
/// ceiling is 15 keys was ending up with room for 28. Asking for
/// `MAX_KEYS + 1` once lands on 16 and stays.
#[test]
fn building_a_tree_has_a_known_allocation_count() {
    const N: usize = 10_000;
    let mut held_after = 0usize;
    let (n, _) = count(|| {
        let mut t = RankTree::new();
        for i in 0..N {
            t.insert(i as u64);
        }
        held_after = LIVE_BYTES.load(Ordering::Relaxed);
        std::hint::black_box(&t);
    });
    // The floor: a run that allocated nothing did not build a tree, and
    // would satisfy any upper bound.
    assert!(n > N / 100, "only {n} allocations for {N} inserts — the counter is not counting");
    // The recorded cost. Tighten it when the shape improves; a rise is a
    // regression to explain, not a number to bump.
    assert!(n <= 1_600, "{n} allocations for {N} inserts (recorded 1393)");
    // Reserving capacity trades bytes for allocation calls, so both are
    // recorded. Payload alone is 8 bytes a key.
    let payload = N * core::mem::size_of::<u64>();
    println!(
        "{N} inserts: {n} allocations, {held_after} bytes held ({:.2}x payload)",
        held_after as f64 / payload as f64
    );
    assert!(held_after > payload, "held {held_after} < payload {payload} — counter broken");
    assert!(
        held_after <= payload * 4, // recorded 3.65x
        "{held_after} bytes held for {payload} of payload — reserving cost more than recorded"
    );
}
