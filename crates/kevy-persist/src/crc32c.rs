//! CRC32C (Castagnoli) — the record checksum of the AOF v2 envelope.
//!
//! The hardware path lives in `kevy_sys::checksum` (the workspace's one
//! sanctioned unsafe home; aarch64 `crc` / x86_64 SSE4.2, runtime
//! detected). This module is safe-only: a slicing-by-8 table fallback
//! that also carries wasm32, where kevy-sys never links. Same
//! polynomial (reflected 0x82F63B78) iSCSI, ext4 and RocksDB use.

/// CRC32C of `data` (init all-ones, final xor all-ones).
#[inline]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Continue a CRC32C: `crc` is the finished checksum of the preceding
/// bytes (0 for none); the result covers them and `data` together.
#[inline]
pub(crate) fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(crc) = kevy_sys::checksum::try_crc32c_hw_append(crc, data) {
        return crc;
    }
    crc32c_sw(crc, data)
}

/// Slicing-by-8 software fallback: 8 tables built once, 8 bytes per step.
fn crc32c_sw(crc: u32, data: &[u8]) -> u32 {
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
    let mut crc = !crc;
    let (chunks, tail) = data.as_chunks::<8>();
    for c in chunks {
        let lo = u32::from_le_bytes(c[..4].try_into().expect("c is [u8; 8] from as_chunks")) ^ crc;
        let hi = u32::from_le_bytes(c[4..].try_into().expect("c is [u8; 8] from as_chunks"));
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
        assert_eq!(crc32c_sw(0, b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_sw(0, b""), 0);
        let long: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&long), crc32c_sw(0, &long));
        assert_eq!(crc32c(&long[1..]), crc32c_sw(0, &long[1..])); // unaligned
    }

    // a checksum continued piece by piece equals the one-shot checksum, on
    // both the host path and the table path
    #[test]
    fn append_over_pieces_equals_whole() {
        let long: Vec<u8> = (0..4099u32).map(|i| (i % 253) as u8).collect();
        let whole = crc32c(&long);
        for cut in [0, 1, 7, 8, 9, 2048, 4098, 4099] {
            let (a, b) = long.split_at(cut);
            assert_eq!(crc32c_append(crc32c_append(0, a), b), whole, "cut at {cut}");
            assert_eq!(crc32c_sw(crc32c_sw(0, a), b), whole, "sw cut at {cut}");
        }
    }
}
