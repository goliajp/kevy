//! The foreign-drain half of [`Heap`] (child module via `#[path]`,
//! the house pattern) — slots other shards freed coming home in a
//! batch. Split from `heap.rs` for the 500-LOC ceiling; the seam is
//! real: nothing else touches the foreign lists.

use core::ptr::NonNull;

use crate::class;
use crate::segment::{self, NO_CLASS};

use super::Heap;

impl Heap {
    /// Move every slot other shards freed back onto its own span's list.
    pub fn drain_foreign(&mut self) {
        let mut seg = self.segments;
        while !seg.is_null() {
            // SAFETY: live header from our own list.
            let s = unsafe { &*seg };
            let mut node = segment::take_foreign(s);
            while !node.is_null() {
                // SAFETY: foreign entries are slot addresses of this
                // segment, linked through their first word.
                let next = unsafe { node.cast::<*mut u8>().read() };
                // SAFETY: non-null in this branch.
                let p = unsafe { NonNull::new_unchecked(node) };
                // SAFETY: still queued and untouched, so the size the
                // freeing thread recorded is still there.
                let requested = unsafe { segment::foreign_requested(p) };
                let ix = segment::span_index_of(p);
                // SAFETY: the span index came from the address itself.
                let cls = unsafe { (*seg).spans[ix].class };
                if cls != NO_CLASS {
                    let c = cls as usize;
                    self.live_bytes -= requested as u64;
                    self.rounding_bytes -= (class::size_of(c) - requested) as u64;
                    // SAFETY: our segment, exclusive access here.
                    unsafe { self.free_local(NonNull::new_unchecked(seg), p, c) };
                }
                node = next;
            }
            seg = s.next;
        }
    }
}
