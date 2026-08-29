//! Equality, specialised on which variant each side is holding.
//!
//! Split out of `lib.rs` when that file reached the workspace's 500-line
//! ceiling. It is a self-contained concern: `PartialEq` branches on the
//! variant **once** and then compares lengths and bytes directly, rather
//! than going through `as_slice()` twice and re-deriving the discriminator
//! on each call. The other trait impls that need only the public slice view
//! live in `traits.rs`; these are here because they read the union.

use core::slice;

use crate::SmallBytes;
use crate::heap::INLINE_LEN_MAX;

// `Debug`, `PartialOrd`, `Ord`, `Hash`, `AsRef<[u8]>`, `Borrow<[u8]>`,
// `KevyHash`, `From<&[u8]>`, `From<Vec<u8>>` live in `crate::traits` —
// they only need the public `as_slice()` view. `PartialEq` / `Eq` stay
// here because the same-variant fast paths reach into `self.inline` /
// `self.heap` directly.

impl SmallBytes {
    /// Both sides inline: compare tag-lengths, then the inline bytes.
    /// Single call site in [`PartialEq::eq`]; `inline(always)` keeps the
    /// split codegen-identical to the pre-split fused body.
    #[allow(clippy::inline_always)] // see doc above: codegen parity with the pre-split body
    #[inline(always)]
    fn eq_inline_inline(&self, other: &Self, self_tag: u8, other_tag: u8) -> bool {
        let len = self_tag as usize;
        if len != other_tag as usize {
            return false;
        }
        // SAFETY: both in inline variant; first `len` bytes valid.
        let a = unsafe { slice::from_raw_parts(self.inline.data.as_ptr(), len) };
        let b = unsafe { slice::from_raw_parts(other.inline.data.as_ptr(), len) };
        a == b
    }

    /// Both sides heap: compare stored lengths, then the heap bytes.
    /// Single call site in [`PartialEq::eq`]; `inline(always)` as above.
    #[allow(clippy::inline_always)] // see doc above: codegen parity with the pre-split body
    #[inline(always)]
    fn eq_heap_heap(&self, other: &Self) -> bool {
        // SAFETY: both in heap variant.
        let (a_len, b_len) = unsafe { (self.heap.length(), other.heap.length()) };
        if a_len != b_len {
            return false;
        }
        // SAFETY: heap pointers + len are valid.
        let a = unsafe { slice::from_raw_parts(self.heap.ptr.as_ptr(), a_len) };
        let b = unsafe { slice::from_raw_parts(other.heap.ptr.as_ptr(), b_len) };
        a == b
    }
}

impl PartialEq for SmallBytes {
    /// Specialised over the slice form (`as_slice == as_slice`) by branching
    /// on variant **once** and reading the relevant length / pointer pair
    /// directly. Same-variant cases (inline/inline + heap/heap, which are the
    /// only ones produced by a single allocator) skip a redundant `as_slice`
    /// dispatch on each side; the mixed case falls back to the slice form.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: byte 23 (`inline.tag`) is always a valid load in either
        // variant — it's either the inline-length 0..=22 or 0xFF as the
        // heap-discriminator overlap (see crate doc).
        let self_tag = unsafe { self.inline.tag };
        let other_tag = unsafe { other.inline.tag };
        let self_inline = self_tag <= INLINE_LEN_MAX;
        let other_inline = other_tag <= INLINE_LEN_MAX;
        match (self_inline, other_inline) {
            (true, true) => self.eq_inline_inline(other, self_tag, other_tag),
            (false, false) => self.eq_heap_heap(other),
            // Mixed inline/heap: this IS reachable in normal operation.
            // It happens whenever HashMap (or any `==` consumer) compares
            // an inline-length value (len ≤ 22) against a heap-length
            // value (len > 22). Two SmallBytes of different lengths can
            // *collide* on hashbrown's hash + quadratic probe, and the
            // probe checks equality even though the lengths differ. The
            // pre-fix `unreachable!()` here was a logic bug — it assumed
            // the same-arm short-circuits cover all cases, but they only
            // fire when both sides land in the same arm. Different-length
            // collisions correctly fall through here. The right answer
            // is just slice-form equality (which short-circuits on `len`
            // internally), giving `false` whenever the lengths differ.
            _ => self.as_slice() == other.as_slice(),
        }
    }
}
impl Eq for SmallBytes {}
