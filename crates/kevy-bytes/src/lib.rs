//! `SmallBytes` — a 24-byte small-byte-string with inline-SSO optimization.
//!
//! Layout (**little-endian only**): a union of two 24-byte variants, distinguished
//! by the last byte:
//!
//! - **Inline**: `[u8; 23]` data, then `u8` tag holding the inline length
//!   (0..=22). The whole string lives in the value, no allocation.
//! - **Heap**: `NonNull<u8>` ptr (8) + `usize` len (8) + `usize` cap_and_tag (8).
//!   The high byte of `cap_and_tag` overlaps byte 23 of the union — the same
//!   byte as Inline::tag — and is kept fixed at `0xFF` (> 22) as the heap
//!   discriminator. The low 56 bits hold the heap capacity (up to 72 PB).
//!
//! This lets us store every byte string up to 22 bytes — covering the vast
//! majority of Redis-style values — without any pointer-chase, while keeping
//! `size_of::<SmallBytes>() == 24` (same as `Vec<u8>`). Used by `kevy-store`
//! to make `Value::Str(SmallBytes)` fit alongside the boxed collection
//! variants and keep `Entry` at 48 B.

#![warn(missing_docs)]

#[cfg(target_endian = "big")]
compile_error!("kevy-bytes requires little-endian: heap-tag byte overlaps inline length byte");

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::{self, ManuallyDrop};
use std::ptr::NonNull;
use std::slice;

const INLINE_CAP: usize = 23;
const INLINE_LEN_MAX: u8 = (INLINE_CAP - 1) as u8;
const TAG_HEAP_BIT: usize = 0xFFusize << 56;
const CAP_MASK: usize = (1usize << 56) - 1;

#[repr(C)]
#[derive(Copy, Clone)]
struct Inline {
    data: [u8; INLINE_CAP],
    /// 0..=22 = inline length. The heap rep sets this byte to 0xFF via the high
    /// byte of `Heap::cap_and_tag` (overlapping on little-endian).
    tag: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Heap {
    ptr: NonNull<u8>,
    len: usize,
    /// High byte = 0xFF (heap marker, shadows `Inline::tag`); low 56 bits =
    /// capacity (from the source `Vec<u8>` or our own alloc; ≥ len).
    cap_and_tag: usize,
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
            debug_assert!(cap <= CAP_MASK, "Vec capacity exceeds 56-bit field");
            Self {
                heap: Heap {
                    ptr,
                    len,
                    cap_and_tag: TAG_HEAP_BIT | (cap & CAP_MASK),
                },
            }
        }
    }

    fn alloc_heap(bytes: &[u8]) -> Self {
        let len = bytes.len();
        let layout = Layout::array::<u8>(len).expect("kevy-bytes: layout overflow");
        // SAFETY: len > 22 ⇒ layout.size() > 0 (alloc requires non-zero size).
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
            heap: Heap {
                ptr,
                len,
                cap_and_tag: TAG_HEAP_BIT | (len & CAP_MASK),
            },
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
            unsafe { self.heap.len }
        }
    }

    /// Whether `len() == 0`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
            unsafe { slice::from_raw_parts(self.heap.ptr.as_ptr(), self.heap.len) }
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
                    self.heap.len,
                    self.heap.cap_and_tag & CAP_MASK,
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
            let cap = self.heap.cap_and_tag & CAP_MASK;
            let layout = Layout::array::<u8>(cap).expect("kevy-bytes: drop layout");
            dealloc(self.heap.ptr.as_ptr(), layout);
        }
    }
}

impl Clone for SmallBytes {
    fn clone(&self) -> Self {
        Self::from_slice(self.as_slice())
    }
}

impl fmt::Debug for SmallBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Match Vec<u8>'s Debug ("[1, 2, 3]" form).
        f.debug_list().entries(self.as_slice().iter()).finish()
    }
}

impl PartialEq for SmallBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for SmallBytes {}

impl PartialOrd for SmallBytes {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SmallBytes {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}

impl Hash for SmallBytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl AsRef<[u8]> for SmallBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::borrow::Borrow<[u8]> for SmallBytes {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

/// `KevyHash` agrees with the byte-slice impl, so a `KevyMap<SmallBytes, V>`
/// can be queried with `&[u8]` (via `Borrow<[u8]>`) and the hash matches.
impl kevy_hash::KevyHash for SmallBytes {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        self.as_slice().kevy_hash()
    }
}

impl From<&[u8]> for SmallBytes {
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl From<Vec<u8>> for SmallBytes {
    fn from(vec: Vec<u8>) -> Self {
        Self::from_vec(vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_align() {
        assert_eq!(mem::size_of::<SmallBytes>(), 24);
        assert_eq!(mem::align_of::<SmallBytes>(), mem::align_of::<usize>());
    }

    #[test]
    fn empty_is_inline() {
        let s = SmallBytes::new();
        assert!(s.is_inline());
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s.as_slice(), b"");
    }

    #[test]
    fn inline_one_byte() {
        let s = SmallBytes::from_slice(b"x");
        assert!(s.is_inline());
        assert_eq!(s.len(), 1);
        assert_eq!(s.as_slice(), b"x");
    }

    #[test]
    fn inline_at_boundary_22() {
        let v: Vec<u8> = (0u8..22).collect();
        let s = SmallBytes::from_slice(&v);
        assert!(s.is_inline());
        assert_eq!(s.len(), 22);
        assert_eq!(s.as_slice(), v);
    }

    #[test]
    fn heap_at_boundary_23() {
        let v: Vec<u8> = (0u8..23).collect();
        let s = SmallBytes::from_slice(&v);
        assert!(!s.is_inline());
        assert_eq!(s.len(), 23);
        assert_eq!(s.as_slice(), v);
    }

    #[test]
    fn heap_large() {
        let v: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
        let s = SmallBytes::from_slice(&v);
        assert!(!s.is_inline());
        assert_eq!(s.len(), 4096);
        assert_eq!(s.as_slice(), v.as_slice());
    }

    #[test]
    fn from_vec_inline() {
        let s = SmallBytes::from_vec(vec![1u8, 2, 3]);
        assert!(s.is_inline());
        assert_eq!(s.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn from_vec_heap_reuses_alloc() {
        let mut v: Vec<u8> = (0u8..100).collect();
        v.reserve(200);
        let ptr_before = v.as_ptr();
        let cap_before = v.capacity();
        let s = SmallBytes::from_vec(v);
        assert!(!s.is_inline());
        // SAFETY: we know it's heap; peek to verify pointer reuse.
        unsafe {
            assert_eq!(s.heap.ptr.as_ptr() as *const u8, ptr_before);
            assert_eq!(s.heap.cap_and_tag & CAP_MASK, cap_before);
        }
    }

    #[test]
    fn into_vec_inline_copies() {
        let s = SmallBytes::from_slice(b"hello");
        let v = s.into_vec();
        assert_eq!(v, b"hello");
    }

    #[test]
    fn into_vec_heap_reuses_alloc() {
        let original: Vec<u8> = (0u8..200).collect();
        let ptr = original.as_ptr();
        let cap = original.capacity();
        let s = SmallBytes::from_vec(original);
        let v = s.into_vec();
        assert_eq!(v.as_ptr(), ptr);
        assert_eq!(v.capacity(), cap);
        assert_eq!(v.len(), 200);
    }

    #[test]
    fn clone_inline() {
        let s = SmallBytes::from_slice(b"abc");
        let c = s.clone();
        assert_eq!(s, c);
        assert!(c.is_inline());
    }

    #[test]
    fn clone_heap() {
        let v: Vec<u8> = (0u8..50).collect();
        let s = SmallBytes::from_slice(&v);
        let c = s.clone();
        assert_eq!(s, c);
        assert!(!c.is_inline());
    }

    #[test]
    fn eq_by_content() {
        let a = SmallBytes::from_slice(b"short");
        let b = SmallBytes::from_slice(b"short");
        assert_eq!(a, b);
        let c: Vec<u8> = (0u8..30).collect();
        let d: Vec<u8> = (0u8..30).collect();
        assert_eq!(SmallBytes::from_slice(&c), SmallBytes::from_slice(&d));
    }

    #[test]
    fn ord_lex() {
        let a = SmallBytes::from_slice(b"abc");
        let b = SmallBytes::from_slice(b"abd");
        assert!(a < b);
    }

    #[test]
    fn debug_format_matches_slice() {
        let s = SmallBytes::from_slice(&[1u8, 2, 3]);
        let dbg = format!("{s:?}");
        let exp = format!("{:?}", &[1u8, 2, 3][..]);
        assert_eq!(dbg, exp);
    }

    #[test]
    fn default_is_empty_inline() {
        let s = SmallBytes::default();
        assert!(s.is_inline());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn drop_heap_does_not_leak_or_double_free() {
        // Loop a bunch to give miri/asan something to catch.
        for n in [23usize, 64, 1024, 65536] {
            let v: Vec<u8> = (0..n).map(|i| (i & 0xFF) as u8).collect();
            let s = SmallBytes::from_slice(&v);
            drop(s);
        }
    }
}
