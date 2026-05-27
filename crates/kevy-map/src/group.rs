//! 16-byte SIMD metadata-scan group — the Swiss-table fast path the SSE2 /
//! NEON path. Probes 16 metadata bytes per iteration instead of 1.
//!
//! Two platform impls plus a scalar fallback, all behind the same
//! `Group` / `BitMask` interface:
//!
//! - **x86_64 SSE2**: `_mm_loadu_si128` + `_mm_cmpeq_epi8` +
//!   `_mm_movemask_epi8`; one bit per slot in the bitmask (stride 1).
//! - **aarch64 NEON**: `vld1q_u8` + `vceqq_u8` + `vshrn_n_u16(_, 4)`
//!   compression to 64-bit mask with four bits per slot (stride 4).
//! - **scalar**: a plain `[u8; 16]` copy + byte-by-byte compare; one
//!   bit per slot.
//!
//! `Group::WIDTH = 16`. Callers ensure `MIN_CAP ≥ 16` so a 16-byte
//! load always lands inside the metadata array.

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::*;

/// 16-byte SIMD chunk of metadata bytes.
#[cfg(target_arch = "x86_64")]
#[derive(Copy, Clone)]
pub(crate) struct Group(__m128i);

#[cfg(target_arch = "aarch64")]
#[derive(Copy, Clone)]
pub(crate) struct Group(uint8x16_t);

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[derive(Copy, Clone)]
pub(crate) struct Group([u8; 16]);

/// Bitmask of slot matches inside a [`Group`]. Iteration order is
/// least-significant-bit-first (i.e., low slot indices first).
#[derive(Copy, Clone)]
pub(crate) struct BitMask(u64);

impl Group {
    /// Number of slots per group. Power-of-two so a `cap` of a
    /// power-of-two is always a clean multiple.
    #[allow(dead_code)] // used by map.rs once probe rewrites land
    pub const WIDTH: usize = 16;

    /// Load 16 metadata bytes starting at `ptr`. Unaligned-safe (we
    /// always use unaligned loads — SSE2's `_mm_loadu_si128` and
    /// aarch64's `vld1q_u8` both tolerate unaligned addresses).
    ///
    /// # Safety
    /// `ptr` must point to at least 16 readable bytes.
    #[inline]
    pub(crate) unsafe fn load(ptr: *const u8) -> Self {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: caller's invariant says 16 bytes are readable.
        unsafe {
            Self(_mm_loadu_si128(ptr as *const __m128i))
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: caller's invariant says 16 bytes are readable.
        unsafe {
            Self(vld1q_u8(ptr))
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let mut buf = [0u8; 16];
            // SAFETY: caller's invariant says 16 bytes are readable; buf is
            // 16 bytes writable, disjoint.
            unsafe { core::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), 16) };
            Self(buf)
        }
    }

    /// Bitmask of byte positions where the group equals `b`.
    #[inline]
    pub(crate) fn match_byte(self, b: u8) -> BitMask {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 intrinsics on owned `__m128i`; no memory access.
        unsafe {
            let bcast = _mm_set1_epi8(b as i8);
            let eq = _mm_cmpeq_epi8(self.0, bcast);
            BitMask(_mm_movemask_epi8(eq) as u32 as u64)
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON intrinsics on owned `uint8x16_t`; no memory access.
        unsafe {
            let bcast = vdupq_n_u8(b);
            let eq = vceqq_u8(self.0, bcast); // each lane: 0xFF if eq, 0x00 if ne
            // Compress 16 lanes into a 64-bit value with 4 bits per source
            // lane via `vshrn_n_u16(_, 4)`. The raw output has each matching
            // input byte's nibble set to 0xF (and 0x0 for misses). We then
            // mask with `0x1111_1111_1111_1111` to keep only the **low bit**
            // of each nibble — i.e. one representative bit per slot at
            // position `4 * slot_index`. That makes the BitMask iterator
            // semantics identical across x86_64 (stride 1) and aarch64
            // (stride 4): `mask & (mask - 1)` cleanly removes one slot.
            let eq16 = vreinterpretq_u16_u8(eq);
            let shrn = vshrn_n_u16(eq16, 4);
            let raw = vget_lane_u64(vreinterpret_u64_u8(shrn), 0);
            BitMask(raw & 0x1111_1111_1111_1111u64)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let mut mask: u64 = 0;
            for (i, &m) in self.0.iter().enumerate() {
                if m == b {
                    mask |= 1u64 << i;
                }
            }
            BitMask(mask)
        }
    }
}

impl BitMask {
    /// Iterate slot indices (0..16) of set bits, low → high.
    #[inline]
    pub(crate) fn iter(self) -> BitMaskIter {
        BitMaskIter(self.0)
    }

    /// True if no slots match.
    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Index of the lowest set slot, or `None` if empty.
    #[inline]
    pub(crate) fn lowest_set(self) -> Option<usize> {
        if self.0 == 0 {
            None
        } else {
            Some(BIT_TO_SLOT(self.0.trailing_zeros() as usize))
        }
    }
}

pub(crate) struct BitMaskIter(u64);

impl Iterator for BitMaskIter {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.0 == 0 {
            return None;
        }
        let bit = self.0.trailing_zeros() as usize;
        // Clear the lowest set bit (Brian-Kernighan).
        self.0 &= self.0 - 1;
        Some(BIT_TO_SLOT(bit))
    }
}

/// Convert bit position inside the BitMask to slot index in [0, 16).
///
/// - x86_64 SSE2 + scalar: 1 bit per slot ⇒ slot = bit.
/// - aarch64 NEON 4-bit-per-slot compression ⇒ slot = bit / 4.
#[cfg(any(target_arch = "x86_64", not(target_arch = "aarch64")))]
#[allow(non_snake_case)]
#[inline]
fn BIT_TO_SLOT(bit: usize) -> usize {
    bit
}

#[cfg(target_arch = "aarch64")]
#[allow(non_snake_case)]
#[inline]
fn BIT_TO_SLOT(bit: usize) -> usize {
    bit >> 2 // divide by 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_finds_all_positions() {
        let buf: [u8; 16] = [
            0xAB, 0x00, 0xAB, 0x01, 0xAB, 0xAB, 0x02, 0xAB,
            0x03, 0x04, 0xAB, 0x05, 0xAB, 0xAB, 0x06, 0xAB,
        ];
        let g = unsafe { Group::load(buf.as_ptr()) };
        let hits: Vec<usize> = g.match_byte(0xAB).iter().collect();
        let want: Vec<usize> = buf
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| (b == 0xAB).then_some(i))
            .collect();
        assert_eq!(hits, want);
    }

    #[test]
    fn match_no_hits_is_empty() {
        let buf = [0u8; 16];
        let g = unsafe { Group::load(buf.as_ptr()) };
        let m = g.match_byte(0x42);
        assert!(m.is_empty());
        assert_eq!(m.iter().count(), 0);
        assert_eq!(m.lowest_set(), None);
    }

    #[test]
    fn match_all_hits() {
        let buf = [0xFFu8; 16];
        let g = unsafe { Group::load(buf.as_ptr()) };
        let hits: Vec<usize> = g.match_byte(0xFF).iter().collect();
        assert_eq!(hits, (0..16).collect::<Vec<_>>());
        assert_eq!(g.match_byte(0xFF).lowest_set(), Some(0));
    }

    #[test]
    fn unaligned_load_works() {
        // Backing buffer of 17 bytes; load at offset 1 (unaligned).
        let buf: [u8; 17] = [
            0xDE, 0xAB, 0x00, 0xAB, 0x01, 0xAB, 0xAB, 0x02,
            0xAB, 0x03, 0x04, 0xAB, 0x05, 0xAB, 0xAB, 0x06, 0xAB,
        ];
        let g = unsafe { Group::load(buf.as_ptr().add(1)) };
        let hits: Vec<usize> = g.match_byte(0xAB).iter().collect();
        // Same as buf[1..17] match_byte(0xAB):
        let want: Vec<usize> = buf[1..17]
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| (b == 0xAB).then_some(i))
            .collect();
        assert_eq!(hits, want);
    }

    #[test]
    fn lowest_set_matches_first_iter() {
        let buf: [u8; 16] = [
            0, 0, 0, 0, 0xAA, 0, 0, 0,
            0xAA, 0, 0, 0, 0, 0, 0, 0,
        ];
        let g = unsafe { Group::load(buf.as_ptr()) };
        let m = g.match_byte(0xAA);
        assert_eq!(m.lowest_set(), Some(4));
        assert_eq!(m.iter().next(), Some(4));
    }
}
