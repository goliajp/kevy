//! Which heap owns the segment an address falls in — for any address.
//!
//! Everything else in this crate recovers a segment by masking a pointer,
//! which is only sound for pointers this allocator handed out. A question
//! asked about an arbitrary address (is this worth moving?) cannot mask
//! first and look second: the masked address may not be mapped at all.
//! This radix tree answers first. It is the shape tcmalloc's pagemap and
//! jemalloc's rtree use, at segment granularity: one entry per 4 MiB of
//! address space, holding the owning heap's token or 0.
//!
//! Two levels on 64-bit targets (a static root of leaf pointers, leaves
//! mapped on first use — one leaf covers 32 GiB), one on 32-bit. Writers
//! are the owning heap when it maps or unmaps a segment; readers are any
//! thread, so every slot is atomic.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::os;
use crate::segment::SEGMENT_BYTES;

const SEG_SHIFT: u32 = SEGMENT_BYTES.trailing_zeros();
#[cfg(target_pointer_width = "64")]
const ADDR_BITS: u32 = 48;
#[cfg(not(target_pointer_width = "64"))]
const ADDR_BITS: u32 = usize::BITS;
const KEY_BITS: u32 = ADDR_BITS - SEG_SHIFT;
const ROOT_BITS: u32 = KEY_BITS / 2;
const LEAF_BITS: u32 = KEY_BITS - ROOT_BITS;
const LEAF_LEN: usize = 1 << LEAF_BITS;
const LEAF_BYTES: usize = LEAF_LEN * core::mem::size_of::<AtomicUsize>();

type Leaf = [AtomicUsize; LEAF_LEN];

static ROOT: [AtomicPtr<Leaf>; 1 << ROOT_BITS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; 1 << ROOT_BITS];

/// A fresh token per heap, so two heaps never share one — heap ids are
/// caller-chosen and tests reuse them.
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

pub(crate) fn new_token() -> usize {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// `(root index, leaf index)` of `addr`'s segment, or `None` past the
/// address width the tree covers.
fn key(addr: usize) -> Option<(usize, usize)> {
    if ADDR_BITS < usize::BITS && addr >> ADDR_BITS != 0 {
        return None;
    }
    let k = addr >> SEG_SHIFT;
    Some((k >> LEAF_BITS, k & (LEAF_LEN - 1)))
}

/// The leaf for root slot `r`, mapping it if `create`.
fn leaf(r: usize, create: bool) -> Option<&'static Leaf> {
    let p = ROOT[r].load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: leaves are mapped once and never unmapped.
        return Some(unsafe { &*p });
    }
    if !create {
        return None;
    }
    let fresh = os::map_aligned(LEAF_BYTES.next_multiple_of(os::PAGE), os::PAGE)?;
    let fresh = fresh.as_ptr().cast::<Leaf>();
    match ROOT[r].compare_exchange(
        core::ptr::null_mut(),
        fresh,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        // SAFETY: zeroed anonymous memory is a valid array of atomics at 0.
        Ok(_) => Some(unsafe { &*fresh }),
        Err(won) => {
            // SAFETY: our own mapping, never published.
            unsafe {
                os::unmap(
                    NonNull::new_unchecked(fresh.cast()),
                    LEAF_BYTES.next_multiple_of(os::PAGE),
                )
            };
            // SAFETY: as above, the winner's leaf is permanent.
            Some(unsafe { &*won })
        }
    }
}

/// Record that the segment at `base` belongs to the heap holding `token`
/// (0 clears it). `false` if the tree could not map a leaf.
pub(crate) fn set(base: usize, token: usize) -> bool {
    let Some((r, i)) = key(base) else { return false };
    let Some(l) = leaf(r, token != 0) else { return token == 0 };
    l[i].store(token, Ordering::Release);
    true
}

/// The token of the heap owning the segment `addr` falls in, 0 if none.
pub(crate) fn owner(addr: usize) -> usize {
    let Some((r, i)) = key(addr) else { return 0 };
    leaf(r, false).map_or(0, |l| l[i].load(Ordering::Acquire))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_answers_its_segments_owner_and_nothing_else() {
        let base = 7 * SEGMENT_BYTES;
        let t = new_token();
        assert!(set(base, t));
        assert_eq!(owner(base + 12_345), t);
        assert_eq!(owner(base + SEGMENT_BYTES), 0, "the next segment is nobody's");
        assert_eq!(owner(usize::MAX), 0, "past the covered width");
        assert!(set(base, 0));
        assert_eq!(owner(base), 0);
    }

    #[test]
    fn a_region_no_segment_ever_used_answers_nobody_and_clears_without_a_leaf() {
        // the last root slot: above every user-space mapping
        let r = (1 << ROOT_BITS) - 1;
        let far = r << (SEG_SHIFT + LEAF_BITS);
        assert_eq!(owner(far + 12_345), 0);
        assert!(set(far, 0), "clearing an absent entry succeeds");
        assert!(ROOT[r].load(Ordering::Acquire).is_null(), "a clear maps no leaf");
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_segment_past_the_covered_width_cannot_be_recorded() {
        assert!(!set(1 << ADDR_BITS, new_token()));
    }
}
