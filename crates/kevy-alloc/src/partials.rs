//! Heap-local recycling structures: the hot cache and the partial-span
//! rings.
//!
//! The legacy profile forced this (the mmap-lock finding's follow-up):
//! with the 16–32 KiB classes a span holds 2–8 slots, so churn exhausts
//! one every few allocations, and the slow path's two O(segments) scans
//! put `Heap::alloc` at 6 % of server self time. This is mimalloc's
//! page-queue-per-class, sized as a ring because entries may go stale —
//! a span can be emptied, reclaimed and reassigned to another class
//! after its entry was pushed — so the pop site validates against the
//! span's live metadata and discards liars. The scans stay behind it as
//! the backstop, which is also why a full ring can simply drop a push.

use core::ptr::NonNull;

use crate::segment::Segment;

/// Entries per class. Eight covers the churn window between slow-path
/// visits; overflow falls back to the scans, losing speed, never slots.
const PARTIAL_RING: usize = 8;

/// A small ring of (segment, span index) candidates believed to have
/// room.
#[derive(Clone, Copy)]
pub(crate) struct PartialRing {
    entries: [(usize, u8); PARTIAL_RING],
    len: u8,
}

impl PartialRing {
    pub(crate) const EMPTY: Self = Self { entries: [(0, 0); PARTIAL_RING], len: 0 };

    /// Register a span that just went full → partial. A full ring
    /// drops the entry — the scans remain as the backstop.
    pub(crate) fn push(&mut self, seg: *mut Segment, ix: usize) {
        if (self.len as usize) < PARTIAL_RING {
            self.entries[self.len as usize] = (seg as usize, ix as u8);
            self.len += 1;
        }
    }

    /// Take the most recently registered candidate. The caller must
    /// validate it — entries lie after a span is reassigned.
    pub(crate) fn pop(&mut self) -> Option<(*mut Segment, usize)> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let (s, ix) = self.entries[self.len as usize];
        Some((s as *mut Segment, ix as usize))
    }
}

/// Slots a class keeps in the heap-local hot cache. Sixteen bounds the
/// pages the cache can pin to a few hundred KB across all classes, and
/// the reclaim tick flushes it anyway.
pub(crate) const CACHE_DEPTH: usize = 16;

/// A per-class stack of recently freed slots, held in the heap itself.
///
/// This is the locality half of a thread cache, added because the
/// header-free finding measured the cost of not having it: every alloc
/// and free touched span metadata 64 KiB–4 MiB away from the data, IPC
/// fell 1.53 → 1.29, and pub/sub paid 16 %. glibc's tcache keeps the
/// hot free list in thread-local memory that stays warm; so does this.
/// A cache hit — the steady state of any alloc/free churn, which is
/// exactly the pub/sub shape — touches no segment line at all.
///
/// The lock-avoidance half of a thread cache is still absent, and still
/// correctly so: this heap has no shared structure to avoid.
#[derive(Clone, Copy)]
pub(crate) struct SlotCache {
    ptrs: [*mut u8; CACHE_DEPTH],
    len: u8,
}

impl SlotCache {
    pub(crate) const EMPTY: Self = Self { ptrs: [core::ptr::null_mut(); CACHE_DEPTH], len: 0 };

    pub(crate) fn pop(&mut self) -> Option<NonNull<u8>> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        NonNull::new(self.ptrs[self.len as usize])
    }

    pub(crate) fn push(&mut self, p: NonNull<u8>) -> bool {
        if self.len as usize == CACHE_DEPTH {
            return false;
        }
        self.ptrs[self.len as usize] = p.as_ptr();
        self.len += 1;
        true
    }
}
