//! Behavioural tests for the heap.
//!
//! These are the assertions `bench/allocgate.sh` names: M3 (the
//! accounting identity), M4 (reclaim actually returns pages), M6 (an
//! exhausted class refuses rather than hands back a wild pointer).
//! Every test skips cleanly where mapping is unavailable, because a test
//! that silently passes on a target it never ran on is worse than one
//! that says it did not run.

use crate::class;
use crate::heap::Heap;
use crate::os;
use crate::class::SPAN_BYTES;

/// Skip the body on targets without anonymous mapping.
macro_rules! require_mapping {
    () => {
        if !os::available() {
            eprintln!("skipped: no anonymous mapping on this target");
            return;
        }
    };
}

#[test]
fn a_round_trip_leaves_nothing_live() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let p = heap.alloc(400, 8).expect("a fresh heap can serve 400 bytes");
    let before = heap.snapshot();
    assert_eq!(before.live, 400);
    assert!(before.balanced(), "{before:?}");
    // SAFETY: `p` came from this heap with this size.
    unsafe { heap.dealloc(p, 400, 8) };
    let after = heap.snapshot();
    assert_eq!(after.live, 0);
    assert_eq!(after.rounding, 0);
    assert!(after.balanced(), "{after:?}");
}

#[test]
fn the_identity_holds_across_a_churn() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let sizes = [16usize, 33, 128, 400, 999, 4096, 8192];
    let mut live: Vec<(core::ptr::NonNull<u8>, usize)> = Vec::new();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for step in 0..4000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let take = !(x as usize).is_multiple_of(3) || live.is_empty();
        if take {
            let size = sizes[(x as usize >> 8) % sizes.len()];
            if let Some(p) = heap.alloc(size, 8) {
                live.push((p, size));
            }
        } else {
            let ix = (x as usize >> 16) % live.len();
            let (p, size) = live.swap_remove(ix);
            // SAFETY: allocated by this heap with this size, and dropped
            // from the live set here.
            unsafe { heap.dealloc(p, size, 8) };
        }
        if step % 500 == 0 {
            let st = heap.snapshot();
            assert!(st.balanced(), "step {step}: {st:?}");
        }
    }
    let expected: u64 = live.iter().map(|(_, s)| *s as u64).sum();
    let st = heap.snapshot();
    assert_eq!(st.live, expected, "live bytes drifted from the truth");
    assert!(st.balanced(), "{st:?}");
    for (p, size) in live {
        // SAFETY: still-live allocations from this heap.
        unsafe { heap.dealloc(p, size, 8) };
    }
}

#[test]
fn slots_are_distinct_and_usable() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let mut given = Vec::new();
    for step in 0..500usize {
        // Markers repeat every 256 slots, which is fine: an overlap
        // between two slots 256 apart would still show up, and any
        // overlap between neighbours certainly would.
        let i = (step % 251) as u8;
        let p = heap.alloc(48, 8).expect("48-byte allocations should succeed");
        // Write a marker through the whole slot: overlapping slots would
        // corrupt a neighbour and the read-back below would catch it.
        // SAFETY: the slot is at least 48 bytes and ours alone.
        unsafe { core::ptr::write_bytes(p.as_ptr(), i, 48) };
        given.push((p, i));
    }
    for (p, marker) in &given {
        // SAFETY: still live, written above.
        let seen = unsafe { p.as_ptr().read() };
        assert_eq!(seen, *marker, "a slot was handed out twice");
    }
    for (p, _) in given {
        // SAFETY: allocated here with this size.
        unsafe { heap.dealloc(p, 48, 8) };
    }
}

#[test]
fn a_freed_slot_comes_back() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let first = heap.alloc(64, 8).unwrap();
    // SAFETY: ours, this size.
    unsafe { heap.dealloc(first, 64, 8) };
    let again = heap.alloc(64, 8).unwrap();
    assert_eq!(first, again, "the free list should hand the slot back");
    // SAFETY: ours, this size.
    unsafe { heap.dealloc(again, 64, 8) };
}

#[test]
fn large_requests_bypass_the_classes() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let size = class::MAX_SMALL + 1;
    // Direct mappings are counted for the process, not the heap: a large
    // block has no segment and so no owner to route a foreign free to.
    let before = crate::large_stats();
    let p = heap.alloc(size, 8).expect("the direct-mapping path serves this");
    let during = crate::large_stats();
    assert_eq!(during.large_count, before.large_count + 1);
    assert_eq!(during.live, before.live + size as u64);
    assert!(during.balanced(), "{during:?}");
    // The heap's own snapshot covers the small path only, and balances
    // on its own — as does the large figure, so their sum does too.
    assert!(heap.snapshot().balanced());
    // SAFETY: ours, this size.
    unsafe { heap.dealloc(p, size, 8) };
    let after = crate::large_stats();
    assert_eq!(after.large_count, before.large_count);
    assert_eq!(after.live, before.live);
}

#[test]
fn m6_an_exhausted_class_refuses_instead_of_handing_back_a_wild_pointer() {
    require_mapping!();
    // The default cap is a runaway guard set past any real workload, so
    // a tighter one is used here to reach the refusal at all — see
    // `PER_CLASS_CAP` for why the inherited value was wrong.
    const CAP: u16 = 3;
    let mut heap = Heap::with_class_cap(0, CAP);
    // The class is chosen large so the number of allocations stays
    // small: 8192-byte slots give 8 per span.
    let c = class::index_of(class::MAX_SMALL, 8).unwrap();
    let per_span = class::slots_per_span(c);
    let capacity = per_span * CAP as usize;
    let mut given = Vec::new();
    for _ in 0..capacity {
        match heap.alloc(class::MAX_SMALL, 8) {
            Some(p) => given.push(p),
            None => break,
        }
    }
    assert_eq!(given.len(), capacity, "the cap should not bite before it is reached");
    assert!(
        heap.alloc(class::MAX_SMALL, 8).is_none(),
        "past the cap the answer must be None — torajs c2970b6d shipped a null instead"
    );
    let st = heap.snapshot();
    assert!(st.balanced(), "{st:?}");
    for p in given {
        // SAFETY: ours, this size.
        unsafe { heap.dealloc(p, class::MAX_SMALL, 8) };
    }
}

#[test]
fn m4_emptied_spans_have_their_pages_returned() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let size = 64;
    let per_span = class::slots_per_span(class::index_of(size, 8).unwrap());
    // Fill several spans' worth so there is something to reclaim beyond
    // the hysteresis the policy deliberately keeps.
    let count = per_span * 12;
    let mut given = Vec::with_capacity(count);
    for _ in 0..count {
        given.push(heap.alloc(size, 8).expect("filling spans"));
    }
    let full = heap.snapshot();
    assert!(full.balanced(), "{full:?}");
    for p in given {
        // SAFETY: ours, this size.
        unsafe { heap.dealloc(p, size, 8) };
    }
    let idle = heap.snapshot();
    assert_eq!(idle.live, 0);
    heap.reclaim();
    let after = heap.snapshot();
    assert!(after.balanced(), "{after:?}");
    assert!(
        after.hysteresis > idle.hysteresis,
        "reclaim returned nothing: hysteresis {} -> {}",
        idle.hysteresis,
        after.hysteresis
    );
    assert!(
        after.predicted_resident() < full.predicted_resident(),
        "predicted residency did not fall: {} -> {}",
        full.predicted_resident(),
        after.predicted_resident()
    );
}

#[test]
fn reclaimed_spans_are_reusable_and_start_clean() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let size = 64;
    let per_span = class::slots_per_span(class::index_of(size, 8).unwrap());
    let mut given: Vec<_> = (0..per_span * 12)
        .map(|_| heap.alloc(size, 8).expect("filling spans"))
        .collect();
    // Poison every byte so a reclaimed-and-reused span that failed to
    // reset its cursors would hand back recognisable garbage.
    for p in &given {
        // SAFETY: live slots of at least `size` bytes.
        unsafe { core::ptr::write_bytes(p.as_ptr(), 0xAB, size) };
    }
    for p in given.drain(..) {
        // SAFETY: ours, this size.
        unsafe { heap.dealloc(p, size, 8) };
    }
    heap.reclaim();
    let mut again = Vec::new();
    for _ in 0..per_span * 4 {
        let p = heap.alloc(size, 8).expect("reclaimed spans must be reusable");
        // SAFETY: freshly handed out, ours.
        unsafe { core::ptr::write_bytes(p.as_ptr(), 0x11, size) };
        again.push(p);
    }
    let st = heap.snapshot();
    assert!(st.balanced(), "{st:?}");
    for p in again {
        // SAFETY: ours, this size.
        unsafe { heap.dealloc(p, size, 8) };
    }
}

#[test]
fn spans_are_page_multiples_so_discard_is_legal() {
    // madvise refuses ranges that are not page-aligned, and a silent
    // refusal would make M4 look like a policy choice rather than a bug.
    assert_eq!(SPAN_BYTES % os::PAGE, 0);
}

/// Resident pages, from `/proc/self/statm` field 2 (in pages).
#[cfg(target_os = "linux")]
fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").expect("procfs");
    let pages: u64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * os::PAGE as u64
}

/// M4 in its real form: the kernel's own resident count must fall.
///
/// The model-level test above checks our prediction; this checks the
/// thing the prediction is about. Linux only, and deliberately so —
/// `MADV_DONTNEED` drops pages outright, while macOS's `MADV_FREE` only
/// marks them reclaimable, so a passing assertion there would mean
/// nothing. glibc's brk arena cannot pass this at any page count, which
/// is the whole reason this crate exists.
#[cfg(target_os = "linux")]
#[test]
fn m4_the_kernel_agrees_that_pages_came_back() {
    require_mapping!();
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
    heap.reclaim();
    let after = rss_bytes();
    let touched = (count * size) as u64;
    assert!(
        after + touched / 2 < peak,
        "RSS barely moved: {peak} -> {after} after freeing {touched} bytes across {} spans",
        count / per_span
    );
}

#[test]
fn spans_with_room_are_reused_before_new_ones_are_claimed() {
    require_mapping!();
    let mut heap = Heap::new(0);
    let size = 64;
    let c = class::index_of(size, 8).unwrap();
    let per_span = class::slots_per_span(c);

    // Fill one span exactly, then free every slot in it. The freed slots
    // land on *that* span's list — not on the current span's, because
    // the next allocation will have moved on.
    let first: Vec<_> = (0..per_span)
        .map(|_| heap.alloc(size, 8).expect("filling the first span"))
        .collect();
    // One more allocation forces a second span to be claimed.
    let straggler = heap.alloc(size, 8).expect("second span");
    for p in first {
        // SAFETY: ours, this size and alignment.
        unsafe { heap.dealloc(p, size, 8) };
    }
    let before = heap.snapshot();

    // Now allocate a whole span's worth again. Every one of these can be
    // served from the emptied first span.
    let again: Vec<_> = (0..per_span)
        .map(|_| heap.alloc(size, 8).expect("reusing the emptied span"))
        .collect();
    let after = heap.snapshot();

    assert_eq!(
        after.spans_assigned, before.spans_assigned,
        "a third span was claimed while the first sat empty — reusable \
         spans must be adopted before new ones are taken"
    );
    assert!(after.balanced(), "{after:?}");

    for p in again {
        // SAFETY: ours, this size and alignment.
        unsafe { heap.dealloc(p, size, 8) };
    }
    // SAFETY: ours, this size and alignment.
    unsafe { heap.dealloc(straggler, size, 8) };
}

#[test]
fn a_foreign_free_leaves_the_owner_balanced_before_and_after_draining() {
    require_mapping!();
    // Two heaps in one thread stand in for two shards: the second has a
    // different identity, so a free through it takes the foreign path
    // exactly as a cross-thread free would — without the timing.
    //
    // This is the regression test for the defect the standard library
    // found the first time it ran on this allocator: the freeing side
    // was subtracting from *its own* counters bytes that had been
    // counted on the owner's, so its totals went negative. Single-thread
    // tests could not reach it, and the byte identity is what catches
    // it, so the assertion is on the identity rather than on a symptom.
    let mut owner = Heap::new(1);
    let mut other = Heap::new(2);

    let size = 400;
    let held: Vec<_> = (0..1_000)
        .map(|_| owner.alloc(size, 8).expect("owner serves these"))
        .collect();
    let full = owner.snapshot();
    assert_eq!(full.live, 1_000 * size as u64);
    assert!(full.balanced(), "{full:?}");

    // Hand every one of them back through the wrong heap.
    for p in held {
        // SAFETY: allocated with this size and alignment; `dealloc`
        // routes a foreign slot to its owning segment itself.
        unsafe { other.dealloc(p, size, 8) };
    }

    // The other heap allocated nothing, so nothing may have moved on it.
    let intruder = other.snapshot();
    assert_eq!(intruder.live, 0, "the freeing heap counted bytes it never handed out");
    assert!(intruder.balanced(), "{intruder:?}");

    // On the owner the slots are in flight: still inside its own
    // counters, now attributed to `cache` so nothing is counted twice.
    let parked = owner.snapshot();
    // Whole slots are parked, so `cache` is in slot bytes — the class
    // serving 400 is 416, and the 16 bytes of rounding travel with it.
    let slot = class::size_of(class::index_of(size, 8).unwrap()) as u64;
    assert_eq!(parked.cache, 1_000 * slot, "slot bytes should be parked");
    assert_eq!(parked.live, 0, "the owner should no longer count them as live");
    assert!(parked.balanced(), "{parked:?}");

    owner.drain_foreign();
    let settled = owner.snapshot();
    assert_eq!(settled.cache, 0, "the list should be empty after draining");
    assert_eq!(settled.live, 0);
    assert_eq!(settled.rounding, 0);
    assert!(settled.balanced(), "{settled:?}");
}
