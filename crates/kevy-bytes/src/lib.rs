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

const INLINE_CAP: usize = 23;
const INLINE_LEN_MAX: u8 = (INLINE_CAP - 1) as u8;

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
struct Heap {
    ptr: NonNull<u8>,
    len: usize,
    /// High byte = 0xFF (heap marker, shadows `Inline::tag`); low 56 bits =
    /// capacity (from the source `Vec<u8>` or our own alloc; ≥ len).
    cap_and_tag: usize,
}

/// 32-bit Heap rep — `ptr(4)|len(4)|cap(4)|pad(11)|tag(1)`. The dedicated
/// `tag` byte at offset 23 (= `0xFF`) plays the role the 64-bit `cap_and_tag`
/// high byte does, so the discriminator check at offset 23 stays identical
/// across both layouts. Unlocks `wasm32-unknown-unknown` (Wave 3 #7) without
/// touching the 64-bit hot path.
#[cfg(target_pointer_width = "32")]
#[repr(C)]
#[derive(Copy, Clone)]
struct Heap {
    ptr: NonNull<u8>,
    len: u32,
    cap: u32,
    _pad: [u8; 11],
    tag: u8,
}

impl Heap {
    /// Build a Heap rep tagging the discriminator byte to `0xFF`. cfg-gated
    /// so each pointer-width hits its native fields without runtime cost.
    #[cfg(target_pointer_width = "64")]
    #[inline]
    fn new(ptr: NonNull<u8>, len: usize, cap: usize) -> Self {
        debug_assert!(cap <= CAP_MASK, "kevy-bytes: capacity exceeds 56-bit field");
        Self {
            ptr,
            len,
            cap_and_tag: TAG_HEAP_BIT | (cap & CAP_MASK),
        }
    }
    #[cfg(target_pointer_width = "32")]
    #[inline]
    fn new(ptr: NonNull<u8>, len: usize, cap: usize) -> Self {
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
mod tests {
    use super::*;
    use kevy_hash::KevyHash as _;
    use std::hash::{Hash, Hasher};

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
            assert_eq!(s.heap.capacity(), cap_before);
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

    // ---- Effective coverage: trait impls + branch paths ---------------------

    #[test]
    fn eq_is_reflexive_and_symmetric_inline() {
        let a = SmallBytes::from_slice(b"hi");
        let b = SmallBytes::from_slice(b"hi");
        let c = SmallBytes::from_slice(b"no");
        assert_eq!(a, a);
        assert_eq!(a, b);
        assert_eq!(b, a);
        assert_ne!(a, c);
    }

    #[test]
    fn eq_is_reflexive_and_symmetric_heap() {
        let v: Vec<u8> = (0u8..40).collect();
        let a = SmallBytes::from_slice(&v);
        let b = SmallBytes::from_slice(&v);
        let mut w = v.clone();
        w[0] = w[0].wrapping_add(1);
        let c = SmallBytes::from_slice(&w);
        assert_eq!(a, a);
        assert_eq!(a, b);
        assert_eq!(b, a);
        assert_ne!(a, c);
    }

    #[test]
    fn partial_cmp_matches_cmp_inline() {
        let a = SmallBytes::from_slice(b"abc");
        let b = SmallBytes::from_slice(b"abd");
        assert_eq!(a.partial_cmp(&b), Some(std::cmp::Ordering::Less));
        assert_eq!(b.partial_cmp(&a), Some(std::cmp::Ordering::Greater));
        assert_eq!(a.partial_cmp(&a), Some(std::cmp::Ordering::Equal));
        // Same chain via the Ord impl directly.
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Less);
        assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
    }

    #[test]
    fn hash_agrees_with_byte_slice() {
        use std::collections::hash_map::DefaultHasher;
        let v: Vec<u8> = (0u8..40).collect();
        let s = SmallBytes::from_slice(&v);
        let mut h_slice = DefaultHasher::new();
        v.as_slice().hash(&mut h_slice);
        let mut h_sb = DefaultHasher::new();
        s.hash(&mut h_sb);
        // Same byte stream into the Hasher (Hash for [u8] writes len + bytes;
        // ours delegates to as_slice so it matches).
        assert_eq!(h_slice.finish(), h_sb.finish());
    }

    #[test]
    fn kevy_hash_agrees_with_byte_slice() {
        let v: Vec<u8> = (0u8..40).collect();
        let s = SmallBytes::from_slice(&v);
        assert_eq!(
            s.kevy_hash(),
            v.as_slice().kevy_hash(),
            "KevyHash impl must agree with &[u8] so a KevyMap<SmallBytes, V> can be queried by Borrow<[u8]>"
        );
        let small = SmallBytes::from_slice(b"foo");
        assert_eq!(small.kevy_hash(), (b"foo" as &[u8]).kevy_hash());
    }

    #[test]
    fn as_ref_is_zero_copy_view() {
        let s = SmallBytes::from_slice(b"abcdef");
        let r: &[u8] = s.as_ref();
        assert_eq!(r, b"abcdef");
        // Same slice address as as_slice (the impl delegates to as_slice).
        assert!(std::ptr::eq(r.as_ptr(), s.as_slice().as_ptr()));
    }

    #[test]
    fn borrow_lookup_works_in_collection() {
        use std::collections::HashMap;
        let mut m: HashMap<SmallBytes, i32> = HashMap::new();
        m.insert(SmallBytes::from_slice(b"key1"), 1);
        m.insert(SmallBytes::from_slice(b"key2"), 2);
        // Look up by &[u8] thanks to Borrow<[u8]>.
        assert_eq!(m.get(b"key1".as_slice()), Some(&1));
        assert_eq!(m.get(b"key2".as_slice()), Some(&2));
        assert_eq!(m.get(b"none".as_slice()), None);
    }

    #[test]
    fn from_byte_slice_round_trip() {
        let a: SmallBytes = (&b"short"[..]).into();
        assert_eq!(a.as_slice(), b"short");
        let v: Vec<u8> = (0u8..40).collect();
        let b: SmallBytes = v.as_slice().into();
        assert_eq!(b.as_slice(), v.as_slice());
        assert!(!b.is_inline());
    }

    #[test]
    fn from_vec_dispatches_inline_or_heap() {
        // ≤ 22 → inline (copies)
        let inline_src: SmallBytes = vec![1u8, 2, 3].into();
        assert!(inline_src.is_inline());
        assert_eq!(inline_src.as_slice(), &[1, 2, 3]);
        // > 22 → heap (reuses alloc; verified by from_vec_heap_reuses_alloc)
        let v: Vec<u8> = (0u8..30).collect();
        let heap_src: SmallBytes = v.clone().into();
        assert!(!heap_src.is_inline());
        assert_eq!(heap_src.as_slice(), v.as_slice());
    }

    #[test]
    fn clone_heap_keeps_data_and_is_independent() {
        // Cloned heap value must allocate a separate buffer (no shared
        // pointer), so dropping the source doesn't invalidate the clone.
        let v: Vec<u8> = (0u8..50).collect();
        let src = SmallBytes::from_slice(&v);
        let dup = src.clone();
        // SAFETY: both in heap variant by len > 22.
        unsafe {
            assert_ne!(
                src.heap.ptr.as_ptr(),
                dup.heap.ptr.as_ptr(),
                "clone must allocate a fresh buffer"
            );
        }
        drop(src);
        // dup remains valid.
        assert_eq!(dup.as_slice(), v.as_slice());
    }

    #[test]
    fn drop_inline_is_noop() {
        // Just exercise the inline path of Drop (the `if self.is_inline()
        // { return }` early-return); miri checks no UB.
        for &n in &[0usize, 1, 5, 22] {
            let s = SmallBytes::from_slice(&vec![b'x'; n]);
            assert!(s.is_inline());
            drop(s);
        }
    }

    #[test]
    fn into_vec_zero_size_path() {
        // Empty (inline) → into_vec returns empty Vec without panic.
        let s = SmallBytes::new();
        let v = s.into_vec();
        assert!(v.is_empty());
    }

    #[test]
    fn to_vec_copies_inline_and_heap() {
        let inline = SmallBytes::from_slice(b"hi");
        assert_eq!(inline.to_vec(), b"hi");
        let v: Vec<u8> = (0u8..30).collect();
        let heap = SmallBytes::from_slice(&v);
        let copy = heap.to_vec();
        assert_eq!(copy, v);
        // to_vec returns an owned independent Vec; heap can be modified
        // via subsequent operations without affecting the returned Vec.
        // (Just verify equality after going through .to_vec.)
        assert_eq!(heap.as_slice(), v.as_slice());
    }

    // ===== alloc-count test =====
    //
    // The whole point of SmallBytes' SSO is "no heap alloc when payload ≤ 22
    // bytes". We can prove it by swapping in a counting allocator and asserting
    // the inline path produces ZERO Allocator::alloc calls. A heap-bound payload
    // produces at least one. Wrapping the system allocator (not replacing it
    // wholesale with a fake) keeps the test compatible with Rust's std types
    // that the tests themselves use.
    //
    // Concurrency: the global allocator is shared by EVERY thread in the test
    // process, and `cargo test` runs ~30 unrelated tests in this crate in
    // parallel. A simple global flag would attribute their allocs to our
    // measurement window. We instead key the recording on a thread-local so
    // only the test thread *currently inside* `measure_allocs` counts.

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAlloc {
        inner: System,
    }

    thread_local! {
        // `const { Cell::new(...) }` is lazily-zero-init at thread spawn — no
        // heap alloc — so the allocator itself can safely consult them.
        static THREAD_RECORDING: Cell<bool> = const { Cell::new(false) };
        static THREAD_ALLOC_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // `try_with` so if the TLS is being destroyed (process teardown)
            // we still serve the alloc instead of panicking.
            let _ = THREAD_RECORDING.try_with(|r| {
                if r.get() {
                    let _ = THREAD_ALLOC_CALLS.try_with(|c| c.set(c.get() + 1));
                }
            });
            // SAFETY: forwarding to the system allocator with the same layout.
            unsafe { self.inner.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: forwarding to the system allocator with the same layout.
            unsafe { self.inner.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static COUNTING: CountingAlloc = CountingAlloc { inner: System };

    fn measure_allocs<F: FnOnce()>(f: F) -> usize {
        THREAD_ALLOC_CALLS.with(|c| c.set(0));
        THREAD_RECORDING.with(|r| r.set(true));
        f();
        THREAD_RECORDING.with(|r| r.set(false));
        THREAD_ALLOC_CALLS.with(|c| c.get())
    }

    #[test]
    fn inline_payload_does_not_allocate() {
        // Warm + capture: every inline-sized SmallBytes constructor + access
        // must produce zero heap allocations. `INLINE_LEN_MAX` is the max
        // payload length the inline variant can hold (one byte of the
        // INLINE_CAP-byte buffer is the length+discriminant tag).
        let max_inline = INLINE_LEN_MAX as usize;
        let allocs = measure_allocs(|| {
            for n in 0..=max_inline {
                let s = SmallBytes::from_slice(&[0u8; INLINE_CAP][..n]);
                std::hint::black_box(&s);
                std::hint::black_box(s.as_slice());
                std::hint::black_box(s.len());
                let c = s.clone(); // Clone of an inline value is also alloc-free.
                std::hint::black_box(&c);
                drop(c);
                drop(s);
            }
        });
        assert_eq!(
            allocs, 0,
            "expected SSO inline path to be alloc-free, got {allocs} allocs"
        );
    }

    #[test]
    fn heap_payload_does_allocate() {
        // Control: payload just over the inline cap MUST allocate. If this
        // is 0 either SSO bumped its cap silently or the counter is broken —
        // either way the inline-zero assertion above is meaningless.
        let max_inline = INLINE_LEN_MAX as usize;
        let allocs = measure_allocs(|| {
            let s = SmallBytes::from_slice(&[7u8; INLINE_CAP + 8][..max_inline + 1]);
            std::hint::black_box(&s);
            drop(s);
        });
        assert!(
            allocs >= 1,
            "expected the heap path to allocate at least once, got {allocs}"
        );
    }

    /// REAL prod incident (mailrs 2026-06-03): two legitimately-constructed
    /// `SmallBytes` values — one inline (≤22 B) and one heap (>22 B) — get
    /// compared by HashMap on a hash-collision. They have different
    /// lengths, so they land in different union arms. Pre-fix: the
    /// `unreachable!()` on the mixed arm panicked. Post-fix: falls back
    /// to slice-form equality, which short-circuits on length internally
    /// and returns `false` whenever the lengths differ. This is THE real
    /// fix — not a defensive hack. The next test forges the same shape
    /// but is the conceptual root-cause test.
    #[test]
    fn partial_eq_mixed_arm_does_not_panic() {
        use std::mem::ManuallyDrop;

        let inline_hi = SmallBytes::from_slice(b"hi");
        let inline_no = SmallBytes::from_slice(b"no");

        // Forge a heap variant that claims to hold "hi" with len = 2 —
        // invariant-violating, but mechanically possible if the union
        // bytes were ever externally written. The backing Vec stays
        // alive via ManuallyDrop so the forged pointer is valid for
        // the read inside PartialEq.
        let mut storage = ManuallyDrop::new(b"hi".to_vec());
        let ptr = NonNull::new(storage.as_mut_ptr()).expect("non-null Vec");
        let forged = ManuallyDrop::new(SmallBytes {
            heap: Heap::new(ptr, 2, 2),
        });

        // Equal content: must return true, must NOT panic.
        assert_eq!(inline_hi, *forged);
        assert_eq!(*forged, inline_hi);
        // Different content: must return false, must NOT panic.
        assert_ne!(inline_no, *forged);
        assert_ne!(*forged, inline_no);

        // Drop sequence under miri's leak detector:
        //   - `forged` stays in ManuallyDrop forever — SmallBytes::Drop
        //     would dealloc(ptr, Layout(len=2, align=1)) using the
        //     forged cap, but the actual underlying allocation belongs
        //     to `storage` and likely has a different (larger) cap →
        //     wrong-layout dealloc = UB. We never run that Drop.
        //   - `storage` is the real owner; drop it explicitly so the
        //     Vec's allocation is released. After this point any
        //     access through forged.heap.ptr would be a use-after-
        //     free, but we don't touch it.
        let _ = forged;
        // SAFETY: storage hasn't been dropped yet and we won't access it
        // after this; the only outstanding alias (forged.heap.ptr) is
        // intentionally orphaned in `forged` which we never read again.
        unsafe { ManuallyDrop::drop(&mut storage); }
    }

    /// The actual mailrs prod crash shape, reproduced without unsafe:
    /// a legitimately-inline short value compared against a
    /// legitimately-heap long value. Different lengths, both correctly
    /// constructed, but they take different union arms. Pre-fix this
    /// panicked at `unreachable!()`; post-fix it just returns `false`.
    ///
    /// Naturally produced by HashMap probing on hash-collision between
    /// keys of different sizes — `_health_probe` (13 B inline) vs a
    /// longer cement key (>22 B heap) in mailrs's case.
    #[test]
    fn partial_eq_unequal_length_across_inline_heap_is_false() {
        let short_inline = SmallBytes::from_slice(b"_health_probe"); // 13 B
        let long_heap = SmallBytes::from_slice(
            b"this string is definitely longer than twenty-two bytes",
        );
        // Sanity: pre-conditions of the shape.
        assert!(short_inline.is_inline());
        assert!(!long_heap.is_inline());
        // The real test: cross-arm comparison must NOT panic and must
        // return false because the lengths differ.
        assert_ne!(short_inline, long_heap);
        assert_ne!(long_heap, short_inline);
    }
}
