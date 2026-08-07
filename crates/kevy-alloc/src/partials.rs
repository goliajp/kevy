//! Per-class partial-span rings — the slow path's O(1) front door.
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
