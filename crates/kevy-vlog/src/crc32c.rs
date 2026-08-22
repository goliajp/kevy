//! CRC32C (Castagnoli) — the vlog record checksum.
//!
//! A copy of kevy-persist's module (same polynomial, same hw dispatch):
//! kevy-vlog cannot depend on kevy-persist (kevy-persist -> kevy-store ->
//! kevy-vlog would cycle), and the shared part is 40 lines. The hardware
//! path lives in `kevy_sys::checksum` (aarch64 `crc` / x86_64 SSE4.2,
//! runtime detected); this module is the safe slicing-by-8 fallback.

/// CRC32C of `data` (init all-ones, final xor all-ones).
#[inline]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(crc) = kevy_sys::checksum::try_crc32c_hw(data) {
        return crc;
    }
    crc32c_sw(data)
}

/// Slicing-by-8 software fallback: 8 tables built once, 8 bytes per step.
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

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer vector (RFC 3720) + hw/sw agreement on this host's path.
    #[test]
    fn known_answers_and_hw_sw_agreement() {
        assert_eq!(crc32c_sw(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_sw(b""), 0);
        let long: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&long), crc32c_sw(&long));
        assert_eq!(crc32c(&long[1..]), crc32c_sw(&long[1..])); // unaligned
    }
}
