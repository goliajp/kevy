//! Hardware CRC32C — the one place in the workspace allowed to hold the
//! `unsafe` a CPU checksum intrinsic needs. Every other crate forbids
//! unsafe outright; kevy-sys is the designated unsafe boundary, and this
//! module extends that charter from "OS calls" to "CPU instructions":
//! no pointers, no FFI, no aliasing — the only unsafety is "this
//! instruction exists", which the runtime feature detection guarantees.
//!
//! Callers keep their own safe software fallback (and wasm32 targets
//! never link this crate at all), so the contract here is narrow:
//! hardware answer or `None`.

/// CRC32C (Castagnoli, init/final-xor all-ones) of `data` using the CPU's
/// checksum instructions — `None` when this machine has none (the caller
/// falls back to its software table).
#[must_use]
pub fn try_crc32c_hw(data: &[u8]) -> Option<u32> {
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("crc") {
            // SAFETY: the `crc` target feature was just detected at runtime.
            return Some(unsafe { crc32c_aarch64(data) });
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: SSE4.2 was just detected at runtime.
            return Some(unsafe { crc32c_x86(data) });
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = data;
    #[allow(unreachable_code)]
    None
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32c_aarch64(data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut crc = !0u32;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        crc = __crc32cd(crc, u64::from_le_bytes(c.try_into().unwrap()));
    }
    for &b in chunks.remainder() {
        crc = __crc32cb(crc, b);
    }
    !crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_x86(data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = !0u64;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(c.try_into().unwrap()));
    }
    let mut crc = crc as u32;
    for &b in chunks.remainder() {
        crc = _mm_crc32_u8(crc, b);
    }
    !crc
}
