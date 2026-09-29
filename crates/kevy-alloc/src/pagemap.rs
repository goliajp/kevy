//! Per-span occupancy — a bitmap in the segment header, one bit per slot.
//!
//! v1 tracked free slots as a LIFO list threaded *through* the slots
//! themselves. That was the structure M3 killed: a page can only be
//! returned to the OS if no metadata lives inside it, and the free
//! list's next-pointers sat in the exact pages `MADV_DONTNEED` would
//! zero — so reclaim could only ever work on whole spans, and a span
//! returns nothing until all of its (up to 157) slots die together.
//!
//! The bitmap moves every trace of occupancy into the header, which buys
//! three properties at once (RFC §5.1 v2):
//!
//! - **pages are pure data**, so any page whose overlapping slots are
//!   all free is returnable while its neighbours stay live;
//! - **lowest-first allocation densifies** — `alloc_slot` takes the
//!   lowest free bit, so live slots pack low and churn migrates free
//!   space upward into whole pages, *manufacturing* returnable pages
//!   rather than waiting for coincident deaths;
//! - **`free` writes nothing into the slot**, one line fewer touched.
//!
//! Worst case (16 B class) is 4096 slots → 512 B of bitmap; 64 spans of
//! metadata ≈ 34 KB, comfortably inside the 64 KiB header span.
//!
//! ```
//! use kevy_alloc::pagemap::{pages_of_slot, slots_of_page};
//! // 416-byte slots: slot 9 straddles pages 0 and 1, so page 1 is
//! // returnable only once slots 9 through 19 are all free
//! assert_eq!(pages_of_slot(9, 416), (0, 1));
//! assert_eq!(slots_of_page(1, 416, 157), (9, 19));
//! ```

use crate::class::{self, SPAN_BYTES};
use crate::os::PAGE;

/// 4 KiB pages per 64 KiB span.
///
/// ```
/// use kevy_alloc::{class::SPAN_BYTES, os::PAGE, pagemap::PAGES_PER_SPAN};
/// assert_eq!(PAGES_PER_SPAN * PAGE, SPAN_BYTES);
/// ```
pub const PAGES_PER_SPAN: usize = SPAN_BYTES / PAGE;

/// `discarded` with every page set — a whole span handed back at once,
/// which is what retiring an emptied span does.
///
/// # Examples
///
/// One bit per page of the span, and no more — the field is a `u16` and
/// a span is 16 pages, so an off-by-one here would either lose a page or
/// set a bit that names nothing.
///
/// ```
/// use kevy_alloc::pagemap::{ALL_PAGES_DISCARDED, PAGES_PER_SPAN};
///
/// assert_eq!(ALL_PAGES_DISCARDED.count_ones() as usize, PAGES_PER_SPAN);
/// assert_eq!(ALL_PAGES_DISCARDED.trailing_ones() as usize, PAGES_PER_SPAN);
/// ```
pub const ALL_PAGES_DISCARDED: u16 = ((1u32 << PAGES_PER_SPAN) - 1) as u16;

/// Bitmap words: enough for the smallest class (16 B → 4096 slots).
///
/// ```
/// use kevy_alloc::class::{index_of, slots_per_span};
/// use kevy_alloc::pagemap::BITMAP_WORDS;
/// // one bit per slot of the smallest class
/// assert_eq!(BITMAP_WORDS * 64, slots_per_span(index_of(16, 8).unwrap()));
/// ```
pub const BITMAP_WORDS: usize = SPAN_BYTES / 16 / 64;

/// No class assigned — the span is free for any class to take.
///
/// ```
/// use kevy_alloc::{class::NCLASSES, pagemap::NO_CLASS};
/// // never a real class index
/// assert!(usize::from(NO_CLASS) >= NCLASSES);
/// ```
pub const NO_CLASS: u8 = 0xFF;

/// Per-span bookkeeping. Deliberately *not* small: the bitmap is the
/// price of page-granular reclaim, and it lives in the header span,
/// which exists to be spent on exactly this.
///
/// # Examples
///
/// ```
/// use kevy_alloc::{Heap, class::index_of, segment::{self, NO_CLASS}};
/// let mut heap = Heap::new(0);
/// let p = heap.alloc(64, 8).ok_or("no mapping")?;
/// // SAFETY: `p` is a live small slot, so it lies inside a segment.
/// let seg = unsafe { segment::segment_of(p).as_ref() };
/// let span = seg.spans()[segment::span_index_of(p)];
/// assert_eq!(usize::from(span.class()), index_of(64, 8).ok_or("class")?);
/// // the last span of a fresh segment has not been given a class yet
/// assert_eq!(seg.spans()[segment::SPANS_PER_SEGMENT - 1].class(), NO_CLASS);
/// // SAFETY: `p` came from this heap with this size and alignment.
/// unsafe { heap.dealloc(p, 64, 8) };
/// # Ok::<(), &str>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct SpanMeta {
    /// Size class this span serves, or [`NO_CLASS`].
    pub(crate) class: u8,
    /// Lowest bitmap word that may hold a zero bit — a scan cursor,
    /// maintained so lowest-first allocation is O(words-with-no-hole)
    /// rather than O(words).
    hint: u8,
    /// Slots handed out and not yet freed.
    pub(crate) live: u16,
    /// Slots at or above this index have never been handed out; their
    /// pages were never touched and are not resident.
    pub(crate) high_water: u16,
    /// Pages returned to the OS (`MADV_DONTNEED`). Cleared per page when
    /// an allocation lands back in one; set wholesale by
    /// [`Heap::retire_empty_span`](crate::Heap) when the span is emptied
    /// and its pages go back together.
    pub(crate) discarded: u16,
    pub(crate) returned_slots: u16,
    /// Set when this span was emptied and handed back to the free pool,
    /// as opposed to never having been assigned at all.
    ///
    /// Both are `class == NO_CLASS`, and they are opposite kinds of
    /// unassigned: one was never touched, the other was touched and then
    /// either discarded or deliberately kept. Without this bit all three
    /// collapse into one bucket, which is what they did — the identity
    /// balances the same whichever way they fall, so nothing caught it.
    ///
    /// # Examples
    ///
    /// The three states a span with no class can be in, and the two
    /// fields that tell them apart:
    ///
    /// ```
    /// use kevy_alloc::pagemap::{ALL_PAGES_DISCARDED, NO_CLASS};
    ///
    /// // (class, retired, discarded) -> what it is
    /// let never_claimed = (NO_CLASS, false, 0u16);
    /// let given_back = (NO_CLASS, true, ALL_PAGES_DISCARDED);
    /// let held = (NO_CLASS, true, 0u16);
    ///
    /// // All three read as "unassigned", and only `retired` plus the
    /// // discard bitmap separate the one that was never touched from the
    /// // one whose pages went back and the one still resident.
    /// for (class, _, _) in [never_claimed, given_back, held] {
    ///     assert_eq!(class, NO_CLASS);
    /// }
    /// assert!(!never_claimed.1);
    /// assert_ne!(given_back.2, held.2);
    /// ```
    pub(crate) retired: bool,
    /// One bit per slot; set = live (or parked on a foreign list, which
    /// pins the page exactly as a live slot does).
    bitmap: [u64; BITMAP_WORDS],
}

impl SpanMeta {
    /// Size class this span serves, or [`NO_CLASS`].
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// assert_eq!(span.class(), kevy_alloc::pagemap::NO_CLASS);
    /// let c = kevy_alloc::class::index_of(400, 8).ok_or("class")?;
    /// span.reset(c as u8);
    /// assert_eq!(usize::from(span.class()), c);
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn class(&self) -> u8 {
        self.class
    }

    /// Slots handed out and not yet freed.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// span.alloc_slot();
    /// span.alloc_slot();
    /// assert_eq!(span.live(), 2);
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn live(&self) -> u16 {
        self.live
    }

    /// Slots at or above this index have never been handed out; their
    /// pages were never touched and are not resident.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// let i = span.alloc_slot().ok_or("full")?;
    /// span.free_slot(i);
    /// assert_eq!(span.high_water(), 1); // slot 0 was touched once
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn high_water(&self) -> u16 {
        self.high_water
    }

    /// Pages returned to the OS, one bit per page of the span.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// span.alloc_slot();
    /// // page 0 holds a live slot, so it is resident
    /// assert_eq!(span.discarded() & 1, 0);
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn discarded(&self) -> u16 {
        self.discarded
    }

    /// Whether this span was emptied and handed back to the free pool,
    /// as opposed to never having been assigned at all.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// assert!(!span.retired()); // never assigned, not handed back
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn retired(&self) -> bool {
        self.retired
    }

    pub(crate) const fn new() -> Self {
        Self {
            class: NO_CLASS,
            hint: 0,
            live: 0,
            high_water: 0,
            discarded: 0,
            returned_slots: 0,
            retired: false,
            bitmap: [0; BITMAP_WORDS],
        }
    }

    /// Assign this span to a class, forgetting everything it held.
    /// Only legal when nothing in it is live.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// // `span` is a copy of an unassigned span's bookkeeping
    /// let c = kevy_alloc::class::index_of(400, 8).ok_or("class")?;
    /// span.reset(c as u8);
    /// assert_eq!(usize::from(span.class()), c);
    /// assert_eq!(span.live(), 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub fn reset(&mut self, class: u8) {
        debug_assert_eq!(self.live, 0, "resetting a span with live slots");
        *self = Self { class, ..Self::new() };
    }

    /// Slots this span can hold, given its class.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// assert_eq!(span.capacity(), 0); // no class yet
    /// span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// assert_eq!(span.capacity(), 65_536 / 416);
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn capacity(&self) -> u32 {
        if self.class == NO_CLASS {
            return 0;
        }
        class::slots_per_span(self.class as usize) as u32
    }

    /// Take the lowest free slot, or `None` when the span is full.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// assert_eq!((span.alloc_slot(), span.alloc_slot()), (Some(0), Some(1)));
    /// span.free_slot(0);
    /// assert_eq!(span.alloc_slot(), Some(0)); // lowest free slot first
    /// # Ok::<(), &str>(())
    /// ```
    pub fn alloc_slot(&mut self) -> Option<u32> {
        let n = self.capacity();
        let words = (n as usize).div_ceil(64);
        for w in (self.hint as usize)..words {
            let holes = !self.bitmap[w];
            if holes == 0 {
                continue;
            }
            let i = (w as u32) * 64 + holes.trailing_zeros();
            if i >= n {
                // Only reachable in the last word: the free bits there
                // are past the slot count, so the span is full.
                return None;
            }
            self.bitmap[w] |= 1u64 << (i % 64);
            self.hint = w as u8;
            self.live += 1;
            if i as u16 >= self.high_water {
                self.high_water = i as u16 + 1;
            }
            return Some(i);
        }
        None
    }

    /// Claim every free bit of the lowest holed word for local
    /// handout: the bits are marked live in the bitmap (a claimed bit
    /// pins its pages exactly as a live slot does, which is what makes
    /// the claim invisible to reclaim), and the caller hands them out
    /// from its own copy without touching this header again. Returns
    /// `(word_index, claimed_mask)`, or `None` when the span is full.
    ///
    /// The far-line arithmetic this exists for: one header round-trip
    /// claims up to 64 slots, so the per-allocation touch that
    /// profiled at 17.3% of collection-write self time amortizes
    /// 64:1. Position-awareness coarsens from bit to word — the claim
    /// still takes the LOWEST holed word, so densification's
    /// lowest-first semantics survive at word granularity.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// span.alloc_slot(); // slot 0
    /// // the rest of word 0 is claimed in one go
    /// assert_eq!(span.claim_word(), Some((0, !1u64)));
    /// assert_eq!(span.live(), 64);
    /// # Ok::<(), &str>(())
    /// ```
    pub fn claim_word(&mut self) -> Option<(u8, u64)> {
        let n = self.capacity();
        let words = (n as usize).div_ceil(64);
        for w in (self.hint as usize)..words {
            let valid = if (w + 1) * 64 <= n as usize {
                !0u64
            } else {
                (1u64 << (n as usize - w * 64)) - 1
            };
            let holes = !self.bitmap[w] & valid;
            if holes == 0 {
                continue;
            }
            self.bitmap[w] |= holes;
            self.live += holes.count_ones() as u16;
            let hi = (w as u32) * 64 + (63 - holes.leading_zeros());
            if hi as u16 >= self.high_water {
                self.high_water = hi as u16 + 1;
            }
            self.hint = w as u8;
            return Some((w as u8, holes));
        }
        None
    }

    /// Return the bits of a claimed word that were never handed out
    /// (or were handed out and locally freed). The exact inverse of
    /// the claim's marking; the hint walks back so lowest-first
    /// allocation sees the holes again.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// let (w, claimed) = span.claim_word().ok_or("full")?;
    /// span.retire_word(w, claimed & !1); // slot 0 was handed out, the rest were not
    /// assert_eq!(span.live(), 1);
    /// assert_eq!(span.alloc_slot(), Some(1));
    /// # Ok::<(), &str>(())
    /// ```
    pub fn retire_word(&mut self, w: u8, unused: u64) {
        debug_assert_eq!(
            self.bitmap[w as usize] & unused,
            unused,
            "retiring bits that were not claimed"
        );
        self.bitmap[w as usize] &= !unused;
        self.live -= unused.count_ones() as u16;
        if w < self.hint {
            self.hint = w;
        }
    }

    /// Mark slot `i` free.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// let i = span.alloc_slot().ok_or("full")?;
    /// span.free_slot(i);
    /// assert!(!span.is_live(i));
    /// assert_eq!(span.live(), 0);
    /// # Ok::<(), &str>(())
    /// ```
    pub fn free_slot(&mut self, i: u32) {
        let w = (i / 64) as usize;
        let m = 1u64 << (i % 64);
        debug_assert!(self.bitmap[w] & m != 0, "double free of slot {i}");
        self.bitmap[w] &= !m;
        self.live -= 1;
        if (w as u8) < self.hint {
            self.hint = w as u8;
        }
    }

    /// Whether slot `i` is live (or parked foreign, which pins pages
    /// identically).
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(400, 8).ok_or("class")? as u8);
    /// let i = span.alloc_slot().ok_or("full")?;
    /// assert!(span.is_live(i));
    /// assert!(!span.is_live(i + 1));
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn is_live(&self, i: u32) -> bool {
        self.bitmap[(i / 64) as usize] & (1u64 << (i % 64)) != 0
    }

    /// Whether any slot in `first..=last` is live.
    ///
    /// ```
    /// # let mut heap = kevy_alloc::Heap::new(0); let p = heap.alloc(64, 8).ok_or("no mapping")?;
    /// # let mut span = unsafe { kevy_alloc::segment::segment_of(p).as_ref() }.spans()[63]; // SAFETY: `p` is a live small slot
    /// # span.reset(kevy_alloc::class::index_of(16, 8).ok_or("class")? as u8);
    /// (0..=70).for_each(|_| { span.alloc_slot(); });
    /// (0..70).for_each(|i| span.free_slot(i)); // only slot 70 survives, in word 1
    /// assert!(span.range_has_live(0, 100));
    /// assert!(!span.range_has_live(0, 69));
    /// # Ok::<(), &str>(())
    /// ```
    #[must_use]
    pub fn range_has_live(&self, first: u32, last: u32) -> bool {
        let (fw, lw) = ((first / 64) as usize, (last / 64) as usize);
        for w in fw..=lw {
            let mut mask = !0u64;
            if w == fw {
                mask &= !0u64 << (first % 64);
            }
            if w == lw {
                mask &= !0u64 >> (63 - (last % 64));
            }
            if self.bitmap[w] & mask != 0 {
                return true;
            }
        }
        false
    }
}

/// The pages slot `i` of a `slot_size` class overlaps, inclusive.
///
/// ```
/// use kevy_alloc::pagemap::pages_of_slot;
/// assert_eq!(pages_of_slot(0, 416), (0, 0));
/// assert_eq!(pages_of_slot(9, 416), (0, 1)); // bytes 3744..4160 cross a page
/// ```
#[must_use]
pub fn pages_of_slot(i: u32, slot_size: usize) -> (usize, usize) {
    let start = i as usize * slot_size;
    let end = start + slot_size - 1;
    (start / PAGE, end / PAGE)
}

/// The slots of a `slot_size` class overlapping page `p`, inclusive,
/// clamped to `nslots`.
///
/// ```
/// use kevy_alloc::pagemap::slots_of_page;
/// assert_eq!(slots_of_page(0, 416, 157), (0, 9));
/// // clamped to the span's slot count at the tail
/// assert_eq!(slots_of_page(15, 416, 157), (147, 156));
/// ```
#[must_use]
pub fn slots_of_page(p: usize, slot_size: usize, nslots: u32) -> (u32, u32) {
    let first = (p * PAGE / slot_size) as u32;
    let last = (((p + 1) * PAGE - 1) / slot_size) as u32;
    (first.min(nslots - 1), last.min(nslots - 1))
}

#[cfg(test)]
#[path = "pagemap_tests.rs"]
mod tests;
