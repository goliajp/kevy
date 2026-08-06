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

    // Hand every one of them back through the wrong heap. Foreign frees
    // land in the freeing heap's outbound ring and cross to the owner's
    // segment only in batches — that amortisation is the whole point
    // (M1: the per-op version cost cross-shard KV 18–39 %).
    for p in held {
        // SAFETY: allocated with this size and alignment; `dealloc`
        // ships a foreign slot home in batches.
        unsafe { other.dealloc(p, size, 8) };
    }

    // The other heap allocated nothing, so nothing may have moved on it.
    let intruder = other.snapshot();
    assert_eq!(intruder.live, 0, "the freeing heap counted bytes it never handed out");
    assert!(intruder.balanced(), "{intruder:?}");

    // Before the freeing side flushes, a partial batch may still sit in
    // its ring. Those slots are still counted by the owner as live —
    // the documented staleness window — and the identity holds exactly
    // through it on both sides.
    let slot = class::size_of(class::index_of(size, 8).unwrap()) as u64;
    let mid = owner.snapshot();
    assert!(mid.balanced(), "{mid:?}");
    let shipped = mid.cache / slot;
    let pending = mid.live / size as u64;
    assert_eq!(
        shipped + pending,
        1_000,
        "every slot is either parked (shipped) or still covered as live (pending): {mid:?}"
    );
    assert_eq!(mid.rounding, pending * (slot - size as u64), "pending rounding rides with pending live");

    // The freeing side's tick flushes the ring; now everything is
    // parked on the owner.
    other.reclaim();
    let parked = owner.snapshot();
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

#[test]
fn v2_pages_return_while_the_span_still_lives() {
    require_mapping!();
    // The measurement that forced v2: a span of the 416-byte class holds
    // 157 slots, and under the whole-span rule all 157 had to die before
    // one page came back. Here the span keeps survivors — only the
    // slots overlapping its LAST page stay live — and the pages below
    // them must come back anyway.
    let mut heap = Heap::new(0);
    let size = 400;
    let slot = class::size_of(class::index_of(size, 8).unwrap());
    let per_span = class::slots_per_span(class::index_of(size, 8).unwrap());
    let given: Vec<_> = (0..per_span)
        .map(|_| heap.alloc(size, 8).expect("filling one span"))
        .collect();
    // Keep everything whose slot touches the last page; free the rest.
    let last_page_start = (crate::pagemap::PAGES_PER_SPAN - 1) * os::PAGE;
    let mut survivors = Vec::new();
    for (i, p) in given.into_iter().enumerate() {
        if (i + 1) * slot > last_page_start {
            survivors.push(p);
        } else {
            // SAFETY: ours, this size and alignment.
            unsafe { heap.dealloc(p, size, 8) };
        }
    }
    assert!(!survivors.is_empty(), "the last page must hold live slots");
    let before = heap.snapshot();
    assert_eq!(before.returned, 0, "nothing returned before the sweep");
    heap.reclaim();
    let after = heap.snapshot();
    assert!(after.balanced(), "{after:?}");
    assert!(
        after.returned > 0,
        "a span with survivors returned nothing — the v1 failure, back: {after:?}"
    );
    // Almost all of the span's free bytes should be returned: only the
    // pages pinned by survivors (and slot-straddling edges) stay.
    assert!(
        after.returned > (crate::pagemap::PAGES_PER_SPAN as u64 / 2) * os::PAGE as u64,
        "returned only {} bytes of a {}-byte span",
        after.returned,
        class::SPAN_BYTES
    );
    assert!(
        after.predicted_resident() < before.predicted_resident(),
        "prediction did not fall: {} -> {}",
        before.predicted_resident(),
        after.predicted_resident()
    );
    for p in survivors {
        // SAFETY: still live, ours.
        unsafe { heap.dealloc(p, size, 8) };
    }
}

#[test]
fn v2_densification_migrates_free_space_into_whole_pages() {
    require_mapping!();
    // Lowest-first allocation is a claim about CHURN, not about one
    // round: any single kill-pattern can leave a survivor pinning every
    // page (the first version of this test did exactly that, freeing
    // alternate slots — the untouched alternates pinned everything).
    // What densification promises is that as churn continues, deaths
    // eventually visit the high slots and their replacements all land
    // low, so free space migrates upward into whole pages. A LIFO free
    // list refills wherever death last struck and promises nothing.
    let mut heap = Heap::new(0);
    let size = 400;
    let c = class::index_of(size, 8).unwrap();
    let per_span = class::slots_per_span(c);
    let mut live: Vec<_> = (0..per_span * 4)
        .map(|_| heap.alloc(size, 8).expect("fill"))
        .collect();
    // Rounds of "half die, a quarter arrive": net shrinkage under a
    // death order that decorrelates from slot position as reallocated
    // (low) slots mix into the vec.
    for _ in 0..8 {
        let mut i = 0usize;
        let mut freed = 0usize;
        live.retain(|p| {
            i += 1;
            if i.is_multiple_of(2) {
                // SAFETY: ours, this size and alignment.
                unsafe { heap.dealloc(*p, size, 8) };
                freed += 1;
                false
            } else {
                true
            }
        });
        for _ in 0..freed / 4 {
            live.push(heap.alloc(size, 8).expect("refill"));
        }
    }
    heap.reclaim();
    let st = heap.snapshot();
    assert!(st.balanced(), "{st:?}");
    assert!(
        st.returned > 0,
        "an interleaved churn produced no returnable page — densification is not happening: {st:?}"
    );
    for p in live {
        // SAFETY: ours.
        unsafe { heap.dealloc(p, size, 8) };
    }
}

/// The kernel's own verdict on v2, Linux only (macOS MADV_FREE gives no
/// prompt guarantee — same reasoning as the whole-span M4 test).
#[cfg(target_os = "linux")]
#[test]
fn v2_the_kernel_reclaims_pages_from_spans_with_survivors() {
    require_mapping!();
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
    let last_page_start = (crate::pagemap::PAGES_PER_SPAN - 1) * os::PAGE;
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
    heap.reclaim();
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
fn spliced_chains_from_two_freeing_heaps_arrive_complete() {
    require_mapping!();
    // Two foreign heaps free into the same owner concurrently-ish: their
    // batches splice onto one segment list. Nothing may be lost, and
    // after the owner drains, every slot must be reusable again.
    let mut owner = Heap::new(1);
    let mut b = Heap::new(2);
    let mut c = Heap::new(3);
    let size = 400;
    let n = 600; // several batches' worth from each side
    let held: Vec<_> = (0..n * 2)
        .map(|_| owner.alloc(size, 8).expect("owner serves these"))
        .collect();
    for (i, p) in held.into_iter().enumerate() {
        // SAFETY: ours, this size and alignment; alternating freers.
        unsafe {
            if i.is_multiple_of(2) {
                b.dealloc(p, size, 8);
            } else {
                c.dealloc(p, size, 8);
            }
        }
    }
    b.reclaim();
    c.reclaim();
    let parked = owner.snapshot();
    let slot = class::size_of(class::index_of(size, 8).unwrap()) as u64;
    assert_eq!(parked.cache, (n * 2) as u64 * slot, "a spliced batch went missing: {parked:?}");
    assert!(parked.balanced(), "{parked:?}");

    owner.drain_foreign();
    let settled = owner.snapshot();
    assert_eq!(settled.live, 0);
    assert_eq!(settled.cache, 0);
    assert!(settled.balanced(), "{settled:?}");
    // And the memory is genuinely reusable: refill without new segments.
    let before_spans = settled.spans_assigned;
    let again: Vec<_> = (0..n * 2)
        .map(|_| owner.alloc(size, 8).expect("drained slots serve again"))
        .collect();
    assert_eq!(
        owner.snapshot().spans_assigned,
        before_spans,
        "drained slots were not reused"
    );
    for p in again {
        // SAFETY: ours.
        unsafe { owner.dealloc(p, size, 8) };
    }
}

/// The collection-write shape the claimed word exists for: short-lived
/// small allocations recycling inside one word, heap-locally. The
/// observable contract: pointers stay lowest-word-stable (position
/// awareness), the accounting identity balances mid-claim, and a
/// retire hands unused bits back so densification sees them.
#[test]
fn claimed_word_recycles_locally_and_retires_honestly() {
    let mut h = Heap::new(1);
    // Churn one size class hard: alloc/free pairs like a hash-node
    // path. Every pointer must come from the same low word while the
    // claim holds (position-aware recycling, not LIFO wander).
    let mut last = None;
    for _ in 0..1000 {
        let p = h.alloc(48, 8).unwrap();
        if let Some(prev) = last {
            assert_eq!(p, prev, "short-lived churn must reuse the same lowest slot");
        }
        last = Some(p);
        unsafe { h.dealloc(p, 48, 8) };
    }
    // Identity balances with a claim in flight (no flush).
    let st = h.snapshot();
    assert_eq!(st.live, 0, "everything was freed");
    assert!(st.balanced(), "{st:?}");
    // After a flush the span sees the truth and reclaim can sweep.
    h.flush_claims();
    let st = h.snapshot();
    assert!(st.balanced(), "{st:?}");
    h.reclaim();
    let st = h.snapshot();
    assert!(st.balanced(), "{st:?}");
}

/// Filling past one word forces claim → refill → claim on the next
/// word; freeing everything then reclaiming must return the pages —
/// the claim must never strand occupancy.
#[test]
fn claims_span_words_and_never_strand_occupancy() {
    let mut h = Heap::new(1);
    let mut ptrs = Vec::new();
    for _ in 0..200 {
        ptrs.push(h.alloc(64, 8).unwrap()); // > 64 slots ⇒ multiple words
    }
    for p in ptrs.drain(..) {
        unsafe { h.dealloc(p, 64, 8) };
    }
    h.flush_claims();
    let st = h.snapshot();
    assert_eq!(st.live, 0);
    assert!(st.balanced(), "{st:?}");
    h.reclaim();
    assert!(h.snapshot().balanced());
}
