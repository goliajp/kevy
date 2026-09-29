use super::*;

fn meta_for(size: usize) -> SpanMeta {
    let mut m = SpanMeta::new();
    m.reset(class::index_of(size, 8).unwrap() as u8);
    m
}

#[test]
fn allocation_is_lowest_first_and_exhausts_exactly() {
    let mut m = meta_for(8192);
    let cap = m.capacity();
    for expect in 0..cap {
        assert_eq!(m.alloc_slot(), Some(expect), "not lowest-first");
    }
    assert_eq!(m.alloc_slot(), None, "over-handed past capacity");
    assert_eq!(m.live as u32, cap);
}

#[test]
fn a_freed_low_slot_is_taken_before_a_higher_hole() {
    let mut m = meta_for(400);
    for _ in 0..100 {
        m.alloc_slot();
    }
    m.free_slot(3);
    m.free_slot(97);
    assert_eq!(m.alloc_slot(), Some(3), "densification broken");
    assert_eq!(m.alloc_slot(), Some(97));
}

#[test]
fn range_has_live_sees_across_word_boundaries() {
    let mut m = meta_for(16); // 4096 slots, many words
    for _ in 0..=130 {
        m.alloc_slot();
    }
    for i in 0..=129 {
        m.free_slot(i);
    }
    // Slot 130 is the only survivor, sitting in word 2.
    assert!(m.range_has_live(0, 200));
    assert!(m.range_has_live(130, 130));
    assert!(!m.range_has_live(0, 129));
    assert!(!m.range_has_live(131, 300));
}

#[test]
fn claim_takes_the_lowest_holed_word_and_retire_reverses_it() {
    let mut m = meta_for(400); // 157 slots -> 3 words, last partial
    for _ in 0..64 {
        m.alloc_slot(); // word 0 full
    }
    let (w, mask) = m.claim_word().expect("word 1 has holes");
    assert_eq!(w, 1, "lowest holed word");
    assert_eq!(mask, !0u64, "all 64 bits were free");
    assert_eq!(m.live, 128);
    // The span-side view: word 1 is now full, allocation skips it.
    assert_eq!(m.alloc_slot(), Some(128), "next span alloc lands in word 2");
    m.free_slot(128);
    // Retire half the claim; those bits become allocatable again.
    m.retire_word(w, 0xFFFF_FFFF);
    assert_eq!(m.live, 96);
    assert_eq!(m.alloc_slot(), Some(64), "retired bit is the lowest hole");
}

#[test]
fn claim_respects_the_capacity_edge() {
    let mut m = meta_for(400); // 157 slots: word 2 has 29 valid bits
    for _ in 0..128 {
        m.alloc_slot();
    }
    let (w, mask) = m.claim_word().expect("partial last word");
    assert_eq!(w, 2);
    assert_eq!(mask.count_ones(), 157 - 128, "only valid bits claimed");
    assert_eq!(m.claim_word(), None, "span exhausted");
    assert_eq!(m.live as u32, m.capacity());
}

#[test]
fn page_and_slot_maps_are_inverses() {
    for size in [16usize, 400, 416, 4096, 8192] {
        let slot = class::size_of(class::index_of(size, 8).unwrap());
        let n = (SPAN_BYTES / slot) as u32;
        for p in 0..PAGES_PER_SPAN {
            let (a, b) = slots_of_page(p, slot, n);
            for i in a..=b {
                let (pa, pb) = pages_of_slot(i, slot);
                assert!(
                    pa <= p && p <= pb,
                    "slot {i} of {slot}B claims pages {pa}..={pb}, not {p}"
                );
            }
        }
    }
}
