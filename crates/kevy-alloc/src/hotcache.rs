//! The heap-local hot cache — recently freed slots, tagged by
//! provenance.
//!
//! Split from `heap.rs` for the file-size rule at a real seam: this is
//! the recycling layer, and after v4 it carries a contract of its own.
//! Entries are tagged pointers (bit 0 = the slot belongs to another
//! heap's segment), so a LOCAL pop — the hottest path there is — learns
//! its provenance without touching a single extra line, and a foreign
//! pop knows to tell the slot's permanent accountant.

use core::ptr::NonNull;

/// Slots a class keeps in the heap-local hot cache. Sixteen bounds the
/// pages the cache can pin to a few hundred KB across all classes, and
/// the reclaim tick flushes the local half anyway.
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
/// Entries are tagged pointers: bit 0 set means the slot belongs to
/// another heap's segment (absorbed under the v4 settlement). Slots are
/// ≥ 16-byte aligned, so the bit is free — and tagging at push means a
/// LOCAL pop, the hottest path there is, learns its provenance without
/// touching a single extra line.
#[derive(Clone, Copy)]
pub(crate) struct SlotCache {
    tagged: [usize; CACHE_DEPTH],
    len: u8,
}

const FOREIGN_TAG: usize = 1;

impl SlotCache {
    pub(crate) const EMPTY: Self = Self { tagged: [0; CACHE_DEPTH], len: 0 };

    /// Pop the most recently parked slot: (pointer, was_foreign).
    pub(crate) fn pop(&mut self) -> Option<(NonNull<u8>, bool)> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let t = self.tagged[self.len as usize];
        NonNull::new((t & !FOREIGN_TAG) as *mut u8).map(|p| (p, t & FOREIGN_TAG != 0))
    }

    pub(crate) fn push(&mut self, p: NonNull<u8>, foreign: bool) -> bool {
        if self.len as usize == CACHE_DEPTH {
            return false;
        }
        self.tagged[self.len as usize] = p.as_ptr() as usize | usize::from(foreign);
        self.len += 1;
        true
    }
}

