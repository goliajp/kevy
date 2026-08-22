//! MD5 (RFC 1321), hand-implemented — kevy is zero-dependency, so the
//! one scalar function that needs a hash carries its own. This is the
//! digest for PG's `md5(text) -> text` (lowercase hex), a fingerprint
//! function, not a security primitive: MD5 is cryptographically broken
//! and used here only where PG uses it (checksums, cache keys).
//!
//! Verified against RFC 1321's own test suite (see the tests module).

/// Per-round left-rotate amounts (RFC 1321 §3.4).
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The 64 sine-derived constants `floor(2^32 · abs(sin(i+1)))`.
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// The 32-hex-character lowercase MD5 digest of `input`.
#[must_use]
pub fn md5_hex(input: &[u8]) -> String {
    let [a, b, c, d] = digest(input);
    let mut out = String::with_capacity(32);
    // The digest is emitted little-endian per word (RFC 1321 §3.5).
    for word in [a, b, c, d] {
        for byte in word.to_le_bytes() {
            out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
            out.push(char::from_digit((byte & 0xf) as u32, 16).expect("nibble"));
        }
    }
    out
}

fn digest(input: &[u8]) -> [u32; 4] {
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    // Append 0x80, then zero-pad to 56 mod 64, then the 64-bit length.
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);

    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = round(i, b, c, d);
            let f = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
}

/// The round function and message-word index for step `i` (RFC 1321
/// §3.4's four 16-step rounds).
#[inline]
fn round(i: usize, b: u32, c: u32, d: u32) -> (u32, usize) {
    match i {
        0..=15 => ((b & c) | (!b & d), i),
        16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
        32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
        _ => (c ^ (b | !d), (7 * i) % 16),
    }
}

#[cfg(test)]
mod tests {
    use super::md5_hex;

    // RFC 1321 Appendix A.5 — the seven canonical test vectors.
    #[test]
    fn rfc1321_test_suite() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            md5_hex(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    // A multi-block input (> 64 bytes with a non-trivial tail) exercises
    // the chunk loop and the length-encoding padding boundary.
    #[test]
    fn spans_multiple_blocks() {
        let input = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(md5_hex(input), "9e107d9d372bb6826bd81d3542a419d6");
        let long = vec![b'x'; 1000];
        assert_eq!(md5_hex(&long), "398533d48111e9f664b1f64cb10c4b63");
    }
}
