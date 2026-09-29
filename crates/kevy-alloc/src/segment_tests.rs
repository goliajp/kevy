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
