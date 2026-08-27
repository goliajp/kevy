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
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(target_endian = "big")]
compile_error!("kevy-bytes requires little-endian: heap-tag byte overlaps inline length byte");

mod find_crlf;
mod eq;
mod traits;

mod heap;
pub(crate) use heap::{Heap, INLINE_CAP, INLINE_LEN_MAX, Inline};

pub use find_crlf::find_crlf;

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use alloc::vec::Vec;
use core::mem::{self, ManuallyDrop};
use core::ptr::NonNull;
use core::slice;

/// A 24-byte owned byte string with inline small-string optimization.
///
/// Strings of up to 22 bytes live entirely inside the value (no allocation,
/// no pointer chase); larger strings spill to a heap buffer. The
/// discriminator is a single byte at offset 23 (the tag, which doubles as
/// the inline length 0..=22 OR equals 0xFF when the heap variant is active).
///
/// See the crate root for layout details.
#[repr(C)]
/// # Examples
///
/// Short values live inline; longer ones move to the heap. The API does not
/// change, but `heap_bytes` reports which happened, which is what the
/// keyspace's memory accounting reads.
///
/// ```
/// use kevy_bytes::SmallBytes;
/// let short = SmallBytes::from_slice(b"hello");
/// assert_eq!(short.as_slice(), b"hello");
/// assert_eq!(short.len(), 5);
/// assert_eq!(short.heap_bytes(), 0, "a short value allocates nothing");
///
/// let long = SmallBytes::from_slice(&[b'x'; 100]);
/// assert_eq!(long.len(), 100);
/// assert!(long.heap_bytes() >= 100, "a long value is on the heap");
/// ```
///
/// ```
/// use kevy_bytes::SmallBytes;
/// assert!(SmallBytes::from_slice(b"").is_empty());
/// ```
pub union SmallBytes {
    // pub(crate) so `eq.rs` can branch on the variant directly; the union
    // itself stays private to this crate's own modules.
    pub(crate) inline: Inline,
    pub(crate) heap: Heap,
}

const _: () = {
    assert!(mem::size_of::<SmallBytes>() == 24);
    assert!(mem::align_of::<SmallBytes>() == mem::align_of::<usize>());
};

unsafe impl Send for SmallBytes {}
unsafe impl Sync for SmallBytes {}

impl SmallBytes {
    /// Empty inline `SmallBytes` (zero allocation).
    ///
    /// # Examples
    ///
    /// `const`, so it can seed a static or an array without a run-time
    /// initialiser:
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// static EMPTY: SmallBytes = SmallBytes::new();
    /// assert!(EMPTY.is_empty());
    /// assert_eq!(EMPTY.heap_bytes(), 0);
    /// ```
    pub const fn new() -> Self {
        Self {
            inline: Inline {
                data: [0; INLINE_CAP],
                tag: 0,
            },
        }
    }

    /// Construct from a byte slice — inline if `bytes.len() <= 22`, else heap.
    ///
    /// # Examples
    ///
    /// Twenty-two is the boundary, and it is exact:
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// assert_eq!(SmallBytes::from_slice(&[b'x'; 22]).heap_bytes(), 0);
    /// assert_eq!(SmallBytes::from_slice(&[b'x'; 23]).heap_bytes(), 23);
    /// ```
    pub fn from_slice(bytes: &[u8]) -> Self {
        if bytes.len() <= INLINE_LEN_MAX as usize {
            let mut data = [0u8; INLINE_CAP];
            // SAFETY: bytes.len() ≤ 22 ≤ data.len(); non-overlapping regions.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), data.as_mut_ptr(), bytes.len());
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
    ///
    /// # Examples
    ///
    /// The heap path keeps the vec's own buffer, so a value that arrived as
    /// a `Vec` is stored without a second copy:
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// let v = vec![b'z'; 64];
    /// let addr = v.as_ptr();
    /// let b = SmallBytes::from_vec(v);
    /// assert_eq!(b.as_slice().as_ptr(), addr, "same allocation, not a copy");
    /// ```
    ///
    /// A short vec goes inline instead, and its allocation is released:
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// assert_eq!(SmallBytes::from_vec(vec![b'a'; 4]).heap_bytes(), 0);
    /// ```
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
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(layout)
        };
        // SAFETY: alloc returned a writable region of `len` bytes; source is a
        // disjoint slice.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), len);
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
    ///
    /// # Examples
    ///
    /// The same answer either side of the inline boundary — which is the
    /// point of the type: where the bytes live is not the caller's problem.
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// assert_eq!(SmallBytes::from_slice(&[0u8; 22]).len(), 22);
    /// assert_eq!(SmallBytes::from_slice(&[0u8; 23]).len(), 23);
    /// ```
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
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// assert!(SmallBytes::from_slice(b"").is_empty());
    /// assert!(!SmallBytes::from_slice(b"\0").is_empty(), "a NUL byte is a byte");
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes this value holds on the heap (0 when inline). Lets memory-accounting
    /// callers (e.g. `maxmemory` enforcement) charge only the off-stack footprint
    /// without re-deriving the inline-length threshold.
    ///
    /// # Examples
    ///
    /// This is what `maxmemory` charges, so an inline value must cost zero
    /// — it is already inside the entry the keyspace has counted:
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// assert_eq!(SmallBytes::from_slice(b"user:1").heap_bytes(), 0);
    /// assert_eq!(SmallBytes::from_slice(&[b'x'; 1000]).heap_bytes(), 1000);
    /// ```
    #[inline]
    pub fn heap_bytes(&self) -> usize {
        if self.is_inline() { 0 } else { self.len() }
    }

    /// Borrow the bytes (no allocation; same for inline and heap variants).
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// let b = SmallBytes::from_slice(b"GET");
    /// assert_eq!(b.as_slice(), b"GET");
    /// assert_eq!(SmallBytes::new().as_slice(), b"");
    /// ```
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
    ///
    /// # Examples
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// let b = SmallBytes::from_slice(b"copy me");
    /// assert_eq!(b.to_vec(), b"copy me");
    /// assert_eq!(b.as_slice(), b"copy me", "the original still holds them");
    /// ```
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    /// Consume self and return an owned `Vec<u8>`. The heap path reuses the
    /// existing allocation; the inline path copies into a new vec.
    ///
    /// # Examples
    ///
    /// A heap value hands its buffer straight back, so a round trip through
    /// `SmallBytes` costs no allocation at either end:
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// let v = vec![b'q'; 128];
    /// let addr = v.as_ptr();
    /// assert_eq!(SmallBytes::from_vec(v).into_vec().as_ptr(), addr);
    /// ```
    ///
    /// ```
    /// use kevy_bytes::SmallBytes;
    /// assert_eq!(SmallBytes::from_slice(b"short").into_vec(), b"short");
    /// ```
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
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(layout)
        };
        // SAFETY: src has `len` valid bytes; dst is freshly-allocated for `len`
        // bytes; regions are disjoint.
        unsafe { core::ptr::copy_nonoverlapping(src_ptr, ptr.as_ptr(), len) };
        Self {
            heap: Heap::new(ptr, len, len),
        }
    }
}



#[cfg(test)]
mod tests;
