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
    let (chunks, tail) = data.as_chunks::<8>();
    for c in chunks {
        crc = __crc32cd(crc, u64::from_le_bytes(*c));
    }
    for &b in tail {
        crc = __crc32cb(crc, b);
    }
    !crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_x86(data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = !0u64;
    let (chunks, tail) = data.as_chunks::<8>();
    for c in chunks {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(*c));
    }
    let mut crc = crc as u32;
    for &b in tail {
        crc = _mm_crc32_u8(crc, b);
    }
    !crc
}

/// Slicing-by-8 software fallback: 8 tables built once, 8 bytes per
/// step. Same reflected polynomial (0x82F63B78) as the hardware path.
fn crc32c_sw(data: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static TABLES: OnceLock<[[u32; 256]; 8]> = OnceLock::new();
    let t = TABLES.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for i in 0..256u32 {
            let mut crc = i;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
            }
            t[0][i as usize] = crc;
        }
        for i in 0..256usize {
            for k in 1..8usize {
                t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xFF) as usize];
            }
        }
        t
    });
    let mut crc = !0u32;
    let (chunks, tail) = data.as_chunks::<8>();
    for c in chunks {
        let lo = u32::from_le_bytes(c[..4].try_into().unwrap()) ^ crc;
        let hi = u32::from_le_bytes(c[4..].try_into().unwrap());
        crc = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in tail {
        crc = (crc >> 8) ^ t[0][((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    !crc
}

/// CRC32C of `data`: the hardware path when this ISA has one, the
/// slicing-by-8 table otherwise. One public front so every consumer
/// (AOF envelope, vlog records, immutable segments) speaks the same
/// checksum without re-owning the fallback.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    try_crc32c_hw(data).unwrap_or_else(|| crc32c_sw(data))
}

#[cfg(test)]
mod crc_front_tests {
    #[test]
    fn known_answer_and_hw_sw_agreement() {
        assert_eq!(super::crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(super::crc32c(b""), 0);
        let long: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(super::crc32c(&long), super::crc32c_sw(&long));
        assert_eq!(super::crc32c(&long[3..]), super::crc32c_sw(&long[3..]));
    }
}
