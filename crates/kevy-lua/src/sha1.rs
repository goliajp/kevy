//! Hand-rolled SHA-1 (RFC 3174 / FIPS 180-1).
//!
//! Used by the SCRIPT LOAD / EVALSHA cache key and the
//! `redis.sha1hex` host fn. Kevy's L2 lockdown forbids crates.io
//! third-party deps; SHA-1 is short enough to write once and lint
//! against well-known test vectors.
//!
//! ## NOT a security primitive
//!
//! SHA-1 is broken for collision resistance (SHAttered, 2017+).
//! That doesn't matter here — kevy uses it as a content-addressed
//! cache key the same way Redis does, where collisions would only
//! cause cross-script cache hits (which never produces a security
//! issue) and Redis itself uses SHA-1 for the same reason.
//!
//! If kevy ever needs SHA-1 / SHA-256 for an actual security-bearing
//! purpose, that's a separate `kevy-crypto` stone, not here.

/// Compute the SHA-1 of `data`. Returns the 20-byte digest.
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    // Pre-processing: append `1` bit, then `0` bits until length ≡ 448 (mod 512),
    // then 64-bit big-endian original-length-in-bits.
    let bit_len: u64 = (data.len() as u64) * 8;
    let mut buf: Vec<u8> = Vec::with_capacity(data.len() + 72);
    buf.extend_from_slice(data);
    buf.push(0x80);
    while buf.len() % 64 != 56 {
        buf.push(0);
    }
    buf.extend_from_slice(&bit_len.to_be_bytes());
    debug_assert_eq!(buf.len() % 64, 0);

    for chunk in buf.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, &word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Format a 20-byte SHA-1 digest as 40 lowercase ASCII hex chars.
pub(crate) fn hex(digest: &[u8; 20]) -> [u8; 40] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 40];
    for (i, &byte) in digest.iter().enumerate() {
        out[i * 2] = HEX[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    out
}

/// Parse a 40-character ASCII hex string into a SHA-1 digest.
/// Returns `None` on malformed input (wrong length or non-hex chars).
pub(crate) fn parse_hex(hex_str: &[u8]) -> Option<[u8; 20]> {
    if hex_str.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, pair) in hex_str.chunks_exact(2).enumerate() {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 3174 / FIPS 180-1 — standard SHA-1 test vectors.
    #[test]
    fn empty_string() {
        // SHA1("") = da39a3ee5e6b4b0d3255bfef95601890afd80709
        let d = sha1(b"");
        assert_eq!(
            hex(&d),
            *b"da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn abc() {
        // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        let d = sha1(b"abc");
        assert_eq!(
            hex(&d),
            *b"a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn quick_brown_fox() {
        // SHA1("The quick brown fox jumps over the lazy dog")
        //   = 2fd4e1c67a2d28fced849ee1bb76e7391b93eb12
        let d = sha1(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            hex(&d),
            *b"2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    #[test]
    fn quick_brown_fox_dot() {
        // SHA1("The quick brown fox jumps over the lazy cog")
        //   = de9f2c7fd25e1b3afad3e85a0bd17d9b100db4b3
        let d = sha1(b"The quick brown fox jumps over the lazy cog");
        assert_eq!(
            hex(&d),
            *b"de9f2c7fd25e1b3afad3e85a0bd17d9b100db4b3"
        );
    }

    #[test]
    fn fips180_56_byte_msg() {
        // SHA1("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
        //   = 84983e441c3bd26ebaae4aa1f95129e5e54670f1
        let d = sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex(&d),
            *b"84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn eight_byte_return_1() {
        // openssl says SHA1("return 1") = e0e1f9fabfc9d4800c877a703b823ac0578ff8db
        let d = sha1(b"return 1");
        assert_eq!(
            hex(&d),
            *b"e0e1f9fabfc9d4800c877a703b823ac0578ff8db"
        );
    }

    #[test]
    fn four_byte_input() {
        // openssl: SHA1("1234") = 7110eda4d09e062aa5e4a390b0a572ac0d2c0220
        let d = sha1(b"1234");
        assert_eq!(
            hex(&d),
            *b"7110eda4d09e062aa5e4a390b0a572ac0d2c0220"
        );
    }

    #[test]
    fn fips180_one_million_a() {
        // SHA1("a" × 1_000_000) = 34aa973cd4c4daa4f61eeb2bdbad27316534016f
        let data = vec![b'a'; 1_000_000];
        let d = sha1(&data);
        assert_eq!(
            hex(&d),
            *b"34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn hex_round_trips_through_parse_hex() {
        let d1 = sha1(b"kevy");
        let h = hex(&d1);
        let d2 = parse_hex(&h).expect("valid hex");
        assert_eq!(d1, d2);
    }

    #[test]
    fn parse_hex_rejects_wrong_length() {
        assert!(parse_hex(b"too short").is_none());
        assert!(parse_hex(&[b'a'; 41]).is_none());
    }

    #[test]
    fn parse_hex_rejects_non_hex_chars() {
        assert!(parse_hex(b"zzz3e364706816aba3e25717850c26c9cd0d89d").is_none());
    }

    #[test]
    fn parse_hex_accepts_uppercase() {
        let d1 = sha1(b"abc");
        let lower = hex(&d1);
        let upper: Vec<u8> = lower
            .iter()
            .map(|&b| b.to_ascii_uppercase())
            .collect();
        let d2 = parse_hex(&upper).expect("upper-hex valid");
        assert_eq!(d1, d2);
    }
}
