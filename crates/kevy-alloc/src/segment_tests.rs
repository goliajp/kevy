use super::*;

#[test]
fn the_header_fits_inside_the_span_it_occupies() {
    assert!(
        core::mem::size_of::<Segment>() <= SPAN_BYTES,
        "the header spills out of span 0 into an allocation span"
    );
}

#[test]
fn geometry_is_maskable() {
    assert!(SEGMENT_BYTES.is_power_of_two());
    assert!(SPAN_BYTES.is_power_of_two());
    assert_eq!(SEGMENT_BYTES % SPAN_BYTES, 0);
    assert_eq!(SPANS_PER_SEGMENT, 64);
}

#[test]
fn the_bitmap_header_still_fits_its_span() {
    // v2 made SpanMeta deliberately large — the bitmap is the price
    // of page-granular reclaim, and the header span exists to be
    // spent on exactly this. The bound that matters is the span.
    assert!(core::mem::size_of::<SpanMeta>() >= crate::pagemap::BITMAP_WORDS * 8);
    assert!(core::mem::size_of::<Segment>() <= SPAN_BYTES);
}

#[test]
fn a_spliced_chain_counts_until_the_owner_settles_it_and_take_hands_it_back_whole() {
    let mut owner = crate::Heap::new(21);
    let (p, q) = (owner.alloc(100, 8).unwrap(), owner.alloc(100, 8).unwrap());
    let slot = class::size_of(class::index_of(100, 8).unwrap());
    // SAFETY: `p` is a live small slot of this heap
    let seg = unsafe { segment_of(p).as_ref() };
    assert_eq!(seg.owner(), 21);
    assert!(seg.take_foreign().is_null(), "nothing freed from elsewhere yet");
    assert_eq!((seg.foreign_live(), seg.foreign_bytes()), (0, 0));
    // SAFETY: both are live slots of this class, ours to overwrite as a
    // two-slot chain p -> q, each carrying its requested size
    unsafe {
        p.as_ptr().add(FOREIGN_SIZE_OFFSET).cast::<u32>().write(100);
        q.as_ptr().add(FOREIGN_SIZE_OFFSET).cast::<u32>().write(100);
        p.as_ptr().cast::<*mut u8>().write(q.as_ptr());
        seg.splice_foreign(p.as_ptr(), q.as_ptr(), 200, 2 * slot);
    }
    assert_eq!((seg.foreign_live(), seg.foreign_bytes()), (200, 2 * slot));
    let chain = seg.take_foreign();
    assert_eq!(chain, p.as_ptr(), "the list comes back from its head");
    // SAFETY: `chain` is `p`, whose first word links the chain
    assert_eq!(unsafe { chain.cast::<*mut u8>().read() }, q.as_ptr());
    assert!(seg.take_foreign().is_null(), "a take leaves the list empty");
    assert_eq!(seg.foreign_bytes(), 2 * slot, "taking settles nothing");
    // SAFETY: the same chain, already counted, spliced back unchanged
    unsafe { seg.splice_foreign(p.as_ptr(), q.as_ptr(), 0, 0) };
    owner.drain_foreign();
    assert_eq!((seg.foreign_live(), seg.foreign_bytes()), (0, 0));
    let s = owner.snapshot();
    assert_eq!(s.live, 0);
    assert!(s.balanced(), "{s:?}");
}

#[test]
fn a_span_keeps_its_high_water_through_frees_and_reads_retired_once_swept() {
    let mut heap = crate::Heap::new(22);
    let c = class::index_of(400, 8).unwrap();
    let n = class::slots_per_span(c);
    let held: Vec<NonNull<u8>> = (0..n).map(|_| heap.alloc(400, 8).unwrap()).collect();
    let ix = span_index_of(held[0]);
    // SAFETY: `held[0]` is a live small slot; the heap's first segment
    // stays mapped until the heap drops
    let seg = unsafe { segment_of(held[0]).as_ref() };
    assert!(held.iter().all(|&p| span_index_of(p) == ix), "one span fills before the next");
    assert_eq!(usize::from(seg.spans()[ix].live()), n);
    assert_eq!(usize::from(seg.spans()[ix].high_water()), n);
    assert!(!seg.spans()[ix].retired());
    for p in held {
        // SAFETY: allocated above with this size and alignment
        unsafe { heap.dealloc(p, 400, 8) };
    }
    heap.flush_claims();
    assert_eq!(seg.spans()[ix].live(), 0);
    assert_eq!(usize::from(seg.spans()[ix].high_water()), n, "freeing never lowers it");
    for _ in 0..=crate::PURGE_DELAY {
        heap.reclaim();
    }
    assert!(seg.spans()[ix].retired(), "an emptied span swept past the delay is handed back");
    assert_eq!(seg.spans()[ix].class(), NO_CLASS);
}
