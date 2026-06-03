//! `SmallBytes` — a 24-byte small-byte-string with inline-SSO optimization.
//!
//! Layout (**little-endian only**): a union of two 24-byte variants, distinguished
//! by the byte at offset 23:
//!
//! - **Inline**: `[u8; 23]` data, then `u8` tag holding the inline length
//!   (0..=22). The whole string lives in the value, no allocation.
//! - **Heap (64-bit)**: `NonNull<u8>` ptr (8) + `usize` len (8) + `usize`
//!   cap_and_tag (8). The high byte of `cap_and_tag` overlaps byte 23 of
//!   the union and is fixed at `0xFF` (> 22) as the heap discriminator. The
//!   low 56 bits hold the heap capacity (up to 72 PB).
//! - **Heap (32-bit)**: `NonNull<u8>` ptr (4) + `u32` len (4) + `u32`
//!   cap (4) + 11-byte pad, then `u8` tag fixed at `0xFF`. Same 24-byte
//!   total, same discriminator byte at offset 23 — pointer / len fields
//!   are 32-bit-native so a `wasm32-unknown-unknown` build picks up the
//!   right size without shifting a `usize` past its bit width.
//!
//! The 64-bit layout is the one the kevy server runs on, and is locked
//! against perf-affecting changes (cfg-gated 32-bit alternative lives
//! alongside it without touching any 64-bit code path).
//!
//! This lets us store every byte string up to 22 bytes — covering the vast
//! majority of Redis-style values — without any pointer-chase, while keeping
//! `size_of::<SmallBytes>() == 24` (same as `Vec<u8>`). Used by `kevy-store`
//! to make `Value::Str(SmallBytes)` fit alongside the boxed collection
//! variants and keep `Entry` at 48 B.

#![warn(missing_docs)]

#[cfg(target_endian = "big")]
compile_error!("kevy-bytes requires little-endian: heap-tag byte overlaps inline length byte");

mod traits;

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::mem::{self, ManuallyDrop};
use std::ptr::NonNull;
use std::slice;

pub(crate) const INLINE_CAP: usize = 23;
pub(crate) const INLINE_LEN_MAX: u8 = (INLINE_CAP - 1) as u8;

#[cfg(target_pointer_width = "64")]
const TAG_HEAP_BIT: usize = 0xFFusize << 56;
#[cfg(target_pointer_width = "64")]
const CAP_MASK: usize = (1usize << 56) - 1;

/// Heap-rep marker byte at offset 23. Used by the 32-bit `Heap::new` to
/// set its dedicated `tag` field; the 64-bit path encodes the same byte
/// implicitly via the high byte of `cap_and_tag`.
#[cfg(target_pointer_width = "32")]
const HEAP_TAG_BYTE: u8 = 0xFF;

#[repr(C)]
#[derive(Copy, Clone)]
struct Inline {
    data: [u8; INLINE_CAP],
    /// 0..=22 = inline length. The heap rep sets this byte to 0xFF either via
    /// the high byte of `Heap::cap_and_tag` (64-bit, little-endian overlap)
    /// or as a dedicated `tag` field at offset 23 (32-bit).
    tag: u8,
}

/// 64-bit Heap rep — `ptr|len|cap_and_tag` × usize. High byte of
/// `cap_and_tag` shadows `Inline::tag` (LE) so the discriminator byte at
/// offset 23 = `0xFF`. Locked layout: the kevy server runs here and the
/// perf budget assumes this exact shape.
#[cfg(target_pointer_width = "64")]
#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct Heap {
    pub(crate) ptr: NonNull<u8>,
    pub(crate) len: usize,
    /// High byte = 0xFF (heap marker, shadows `Inline::tag`); low 56 bits =
    /// capacity (from the source `Vec<u8>` or our own alloc; ≥ len).
    pub(crate) cap_and_tag: usize,
}

/// 32-bit Heap rep — `ptr(4)|len(4)|cap(4)|pad(11)|tag(1)`. The dedicated
/// `tag` byte at offset 23 (= `0xFF`) plays the role the 64-bit `cap_and_tag`
/// high byte does, so the discriminator check at offset 23 stays identical
/// across both layouts. Unlocks `wasm32-unknown-unknown` (Wave 3 #7) without
/// touching the 64-bit hot path.
#[cfg(target_pointer_width = "32")]
#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct Heap {
    pub(crate) ptr: NonNull<u8>,
    pub(crate) len: u32,
    pub(crate) cap: u32,
    pub(crate) _pad: [u8; 11],
    pub(crate) tag: u8,
}

impl Heap {
    /// Build a Heap rep tagging the discriminator byte to `0xFF`. cfg-gated
    /// so each pointer-width hits its native fields without runtime cost.
    #[cfg(target_pointer_width = "64")]
    #[inline]
    pub(crate) fn new(ptr: NonNull<u8>, len: usize, cap: usize) -> Self {
        debug_assert!(cap <= CAP_MASK, "kevy-bytes: capacity exceeds 56-bit field");
        Self {
            ptr,
            len,
            cap_and_tag: TAG_HEAP_BIT | (cap & CAP_MASK),
        }
    }
    #[cfg(target_pointer_width = "32")]
    #[inline]
    pub(crate) fn new(ptr: NonNull<u8>, len: usize, cap: usize) -> Self {
        // On 32-bit, `Vec<u8>` is bounded by the 4 GiB address space, so
        // any source `len`/`cap` already fits in `u32`. Debug-assert to
        // catch unexpected callers.
        debug_assert!(
            len <= u32::MAX as usize && cap <= u32::MAX as usize,
            "kevy-bytes: len/cap exceeds u32 on 32-bit platform"
        );
        Self {
            ptr,
            len: len as u32,
            cap: cap as u32,
            _pad: [0; 11],
            tag: HEAP_TAG_BYTE,
        }
    }

    /// Live capacity (always returned as `usize` regardless of underlying
    /// field width).
    #[cfg(target_pointer_width = "64")]
    #[inline]
    fn capacity(&self) -> usize {
        self.cap_and_tag & CAP_MASK
    }
    #[cfg(target_pointer_width = "32")]
    #[inline]
    fn capacity(&self) -> usize {
        self.cap as usize
    }

    /// Live length (always `usize`).
    #[cfg(target_pointer_width = "64")]
    #[inline]
    fn length(&self) -> usize {
        self.len
    }
    #[cfg(target_pointer_width = "32")]
    #[inline]
    fn length(&self) -> usize {
        self.len as usize
    }
}

/// A 24-byte owned byte string with inline small-string optimization.
///
/// Strings of up to 22 bytes live entirely inside the value (no allocation,
/// no pointer chase); larger strings spill to a heap buffer. The
/// discriminator is a single byte at offset 23 (the tag, which doubles as
/// the inline length 0..=22 OR equals 0xFF when the heap variant is active).
///
/// See the crate root for layout details.
#[repr(C)]
pub union SmallBytes {
    inline: Inline,
    heap: Heap,
}

const _: () = {
    assert!(mem::size_of::<SmallBytes>() == 24);
    assert!(mem::align_of::<SmallBytes>() == mem::align_of::<usize>());
};

unsafe impl Send for SmallBytes {}
unsafe impl Sync for SmallBytes {}

impl SmallBytes {
    /// Empty inline `SmallBytes` (zero allocation).
    pub const fn new() -> Self {
        Self {
            inline: Inline {
                data: [0; INLINE_CAP],
                tag: 0,
            },
        }
    }

    /// Construct from a byte slice — inline if `bytes.len() <= 22`, else heap.
    pub fn from_slice(bytes: &[u8]) -> Self {
        if bytes.len() <= INLINE_LEN_MAX as usize {
            let mut data = [0u8; INLINE_CAP];
            // SAFETY: bytes.len() ≤ 22 ≤ data.len(); non-overlapping regions.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.as_mut_ptr(), bytes.len());
            }
            Self {
                inline: Inline {
                    data,
                    tag: bytes.len() as u8,
                },
            }
        } else {
            Self::alloc_heap(bytes)
        }
    }

    /// Take ownership of a `Vec<u8>` — inline if `vec.len() <= 22`, else **reuse
    /// the vec's allocation** (no copy on the heap path).
    pub fn from_vec(vec: Vec<u8>) -> Self {
        if vec.len() <= INLINE_LEN_MAX as usize {
            Self::from_slice(&vec)
        } else {
            let mut v = ManuallyDrop::new(vec);
            // SAFETY: len > 22 ⇒ cap > 0 ⇒ Vec has an allocation, so the pointer
            // is non-null. Vec guarantees a non-null pointer for any allocated
            // Vec (and a dangling-but-non-null for empty, which we don't hit here).
            let ptr = unsafe { NonNull::new_unchecked(v.as_mut_ptr()) };
            let len = v.len();
            let cap = v.capacity();
            Self {
                heap: Heap::new(ptr, len, cap),
            }
        }
    }

    #[inline]
    fn alloc_heap(bytes: &[u8]) -> Self {
        let len = bytes.len();
        // `len > 22` (caller has already taken the heap branch) and `len` is
        // a slice length ⇒ ≤ `isize::MAX` ⇒ well below the `usize::MAX -
        // (align - 1)` bound `from_size_align_unchecked` needs. u8's align is 1.
        // SAFETY: see above.
        let layout = unsafe { Layout::from_size_align_unchecked(len, 1) };
        // SAFETY: layout.size() > 0 (caller's heap branch guarantees len > 22).
        let raw = unsafe { alloc(layout) };
        let ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => handle_alloc_error(layout),
        };
        // SAFETY: alloc returned a writable region of `len` bytes; source is a
        // disjoint slice.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), len);
        }
        Self {
            heap: Heap::new(ptr, len, len),
        }
    }

    /// True when stored inline; the byte at index 23 is the deciding tag in
    /// either rep, so the check is a single load + compare.
    #[inline]
    fn is_inline(&self) -> bool {
        // SAFETY: byte 23 is always initialised — either as Inline::tag (0..=22)
        // or as the high byte of Heap::cap_and_tag (= 0xFF). Reading it through
        // the Inline view is valid in either case (the union is `repr(C)`).
        unsafe { self.inline.tag <= INLINE_LEN_MAX }
    }

    /// Number of bytes stored.
    #[inline]
    pub fn len(&self) -> usize {
        if self.is_inline() {
            // SAFETY: just verified `inline.tag` ≤ 22.
            unsafe { self.inline.tag as usize }
        } else {
            // SAFETY: tag > 22 ⇒ heap variant is active.
            unsafe { self.heap.length() }
        }
    }

    /// Whether `len() == 0`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes this value holds on the heap (0 when inline). Lets memory-accounting
    /// callers (e.g. `maxmemory` enforcement) charge only the off-stack footprint
    /// without re-deriving the inline-length threshold.
    #[inline]
    pub fn heap_bytes(&self) -> usize {
        if self.is_inline() { 0 } else { self.len() }
    }

    /// Borrow the bytes (no allocation; same for inline and heap variants).
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        if self.is_inline() {
            // SAFETY: first `tag` bytes of `data` are valid (zero-init at construction).
            unsafe {
                slice::from_raw_parts(self.inline.data.as_ptr(), self.inline.tag as usize)
            }
        } else {
            // SAFETY: heap variant active; ptr/len originate from a Vec or our own alloc.
            unsafe { slice::from_raw_parts(self.heap.ptr.as_ptr(), self.heap.length()) }
        }
    }

    /// Copy into a fresh `Vec<u8>` (clone semantics).
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    /// Consume self and return an owned `Vec<u8>`. The heap path reuses the
    /// existing allocation; the inline path copies into a new vec.
    pub fn into_vec(self) -> Vec<u8> {
        if self.is_inline() {
            self.as_slice().to_vec()
            // self drops as inline — nothing to free.
        } else {
            // SAFETY: heap variant active.
            let (ptr, len, cap) = unsafe {
                (
                    self.heap.ptr.as_ptr(),
                    self.heap.length(),
                    self.heap.capacity(),
                )
            };
            // Skip our Drop to avoid double-free; Vec::from_raw_parts now owns it.
            let _do_not_drop = ManuallyDrop::new(self);
            // SAFETY: ptr/len/cap originated from either a Vec<u8> (from_vec)
            // or our own `alloc(Layout::array::<u8>(cap))` (alloc_heap, where
            // cap == len) — both meet Vec::from_raw_parts' requirements.
            unsafe { Vec::from_raw_parts(ptr, len, cap) }
        }
    }
}

impl Default for SmallBytes {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SmallBytes {
    fn drop(&mut self) {
        if self.is_inline() {
            return;
        }
        // SAFETY: heap variant active; layout matches the one used at alloc
        // time (either from Vec — Vec uses `Layout::array::<u8>(cap)` — or our
        // own alloc_heap which used the same layout).
        unsafe {
            let cap = self.heap.capacity();
            let layout = Layout::array::<u8>(cap).expect("kevy-bytes: drop layout");
            dealloc(self.heap.ptr.as_ptr(), layout);
        }
    }
}

impl Clone for SmallBytes {
    /// Specialised clone that bypasses `as_slice → from_slice → alloc_heap`'s
    /// two layered length checks. Inline variant is a bitwise union copy (no
    /// branch through the slice path); heap variant goes straight to a single
    /// `alloc + memcpy` keyed on the already-known heap length.
    #[inline]
    fn clone(&self) -> Self {
        if self.is_inline() {
            // SAFETY: `Inline` is `repr(C)` + `Copy`; bitwise copy is sound
            // when the source is currently in the inline variant (the tag
            // byte ≤ 22 is part of the bit pattern we're copying, so the
            // discriminator stays correct).
            unsafe { Self { inline: self.inline } }
        } else {
            // SAFETY: tag > 22 ⇒ heap variant is active.
            unsafe { self.clone_heap() }
        }
    }
}

impl SmallBytes {
    /// Heap-fast-path clone. Caller must have established that `self` is in
    /// the heap variant.
    ///
    /// # Safety
    /// `self.heap` must be the active union variant (i.e. `is_inline()` is
    /// false). `self.heap.ptr` must point to `self.heap.len` valid bytes.
    #[inline]
    unsafe fn clone_heap(&self) -> Self {
        // SAFETY (covers the three `self.heap.*` reads): caller asserts the
        // heap variant is active.
        let (src_ptr, len) = unsafe { (self.heap.ptr.as_ptr(), self.heap.length()) };
        // `len > 22 ⇒ len > 0`, and the high bits are guarded by `CAP_MASK`
        // never letting cap exceed 2^56, well below `isize::MAX`, so the
        // unchecked layout is sound. Allocator alignment for `u8` is 1.
        let layout = unsafe { Layout::from_size_align_unchecked(len, 1) };
        // SAFETY: layout.size() > 0.
        let raw = unsafe { alloc(layout) };
        let ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => handle_alloc_error(layout),
        };
        // SAFETY: src has `len` valid bytes; dst is freshly-allocated for `len`
        // bytes; regions are disjoint.
        unsafe { std::ptr::copy_nonoverlapping(src_ptr, ptr.as_ptr(), len) };
        Self {
            heap: Heap::new(ptr, len, len),
        }
    }
}

// `Debug`, `PartialOrd`, `Ord`, `Hash`, `AsRef<[u8]>`, `Borrow<[u8]>`,
// `KevyHash`, `From<&[u8]>`, `From<Vec<u8>>` live in `crate::traits` —
// they only need the public `as_slice()` view. `PartialEq` / `Eq` stay
// here because the same-variant fast paths reach into `self.inline` /
// `self.heap` directly.

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
            (true, true) => {
                let len = self_tag as usize;
                if len != other_tag as usize {
                    return false;
                }
                // SAFETY: both in inline variant; first `len` bytes valid.
                let a = unsafe {
                    slice::from_raw_parts(self.inline.data.as_ptr(), len)
                };
                let b = unsafe {
                    slice::from_raw_parts(other.inline.data.as_ptr(), len)
                };
                a == b
            }
            (false, false) => {
                // SAFETY: both in heap variant.
                let (a_len, b_len) =
                    unsafe { (self.heap.length(), other.heap.length()) };
                if a_len != b_len {
                    return false;
                }
                // SAFETY: heap pointers + len are valid.
                let a = unsafe {
                    slice::from_raw_parts(self.heap.ptr.as_ptr(), a_len)
                };
                let b = unsafe {
                    slice::from_raw_parts(other.heap.ptr.as_ptr(), b_len)
                };
                a == b
            }
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


#[cfg(test)]
mod tests;
