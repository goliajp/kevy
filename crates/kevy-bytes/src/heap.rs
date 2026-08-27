//! The heap representation behind `SmallBytes`, and the constants that
//! separate it from the inline one.
//!
//! Split out of `lib` in v6, which had reached the 500-line ceiling. The
//! division is the one the module doc already describes: this file is the
//! 24-byte layout — two pointer-width variants and the tag byte that tells
//! them apart — while `lib` is the type built on top of it. Nothing here
//! knows what a byte string is; it knows where the bytes are.

use core::ptr::NonNull;

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
pub(crate) struct Inline {
    pub(crate) data: [u8; INLINE_CAP],
    /// 0..=22 = inline length. The heap rep sets this byte to 0xFF either via
    /// the high byte of `Heap::cap_and_tag` (64-bit, little-endian overlap)
    /// or as a dedicated `tag` field at offset 23 (32-bit).
    pub(crate) tag: u8,
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
    pub(crate) fn capacity(&self) -> usize {
        self.cap_and_tag & CAP_MASK
    }
    #[cfg(target_pointer_width = "32")]
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.cap as usize
    }

    /// Live length (always `usize`).
    #[cfg(target_pointer_width = "64")]
    #[inline]
    pub(crate) fn length(&self) -> usize {
        self.len
    }
    #[cfg(target_pointer_width = "32")]
    #[inline]
    pub(crate) fn length(&self) -> usize {
        self.len as usize
    }
}
