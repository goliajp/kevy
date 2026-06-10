//! CRC16-CCITT (XMODEM) and the Redis-cluster key→slot mapping.
//!
//! Redis Cluster routes every key to one of 16384 slots via
//! `CRC16(hashtag(key)) mod 16384` with the XMODEM parameterisation
//! (poly `0x1021`, init `0`, no reflection, no final xor). kevy's
//! single-node cluster subset speaks the same protocol so stock
//! cluster-aware clients (`redis-benchmark --cluster`, `redis-cli -c`)
//! can discover and address the per-shard ports directly.
//!
//! Like the rest of this crate, **not collision-resistant** against an
//! adversary who chooses keys — CRC16 is trivially invertible. Same
//! single-trust-domain assumption as [`crate::KevyHash`].

/// XMODEM generator polynomial.
const POLY: u16 = 0x1021;

/// 256-entry table, generated at compile time (MSB-first, init 0).
const TABLE: [u16; 256] = make_table();

const fn make_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// CRC16-CCITT (XMODEM) of `bytes`. Check vector: `crc16(b"123456789") == 0x31C3`.
#[inline]
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc = (crc << 8) ^ TABLE[(((crc >> 8) as u8) ^ b) as usize];
    }
    crc
}

/// The Redis-cluster hash slot of `key`: `crc16(hashtag(key)) & 16383`.
///
/// Hashtag rule (Redis spec): if the key contains a `{` followed by a
/// later `}` with at least one byte between them, only the bytes between
/// the **first** `{` and the **first** `}` after it are hashed — so
/// `{user1000}.following` and `{user1000}.followers` land on one slot.
/// An empty `{}` (or no braces) hashes the whole key.
#[inline]
pub fn key_hash_slot(key: &[u8]) -> u16 {
    crc16(hashtag(key)) & 0x3FFF
}

#[inline]
fn hashtag(key: &[u8]) -> &[u8] {
    let Some(open) = key.iter().position(|&b| b == b'{') else {
        return key;
    };
    let rest = &key[open + 1..];
    match rest.iter().position(|&b| b == b'}') {
        Some(close) if close > 0 => &rest[..close],
        _ => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmodem_check_vector() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc16(b""), 0);
    }

    #[test]
    fn slot_is_in_range() {
        for key in [&b"foo"[..], b"key:000000000042", b"", b"\xff\x00\xfe"] {
            assert!(key_hash_slot(key) < 16384);
        }
    }

    #[test]
    fn hashtag_extraction_redis_spec_examples() {
        // {user1000}.following / .followers share the slot of "user1000".
        assert_eq!(
            key_hash_slot(b"{user1000}.following"),
            key_hash_slot(b"{user1000}.followers")
        );
        assert_eq!(key_hash_slot(b"{user1000}.following"), key_hash_slot(b"user1000"));
        // Empty {} → whole key is hashed.
        assert_eq!(hashtag(b"foo{}{bar}"), b"foo{}{bar}");
        // Only the first { … first } after it counts.
        assert_eq!(hashtag(b"foo{{bar}}zap"), b"{bar");
        assert_eq!(hashtag(b"foo{bar}{zap}"), b"bar");
        // Unclosed brace → whole key.
        assert_eq!(hashtag(b"foo{bar"), b"foo{bar");
        assert_eq!(hashtag(b"no_braces"), b"no_braces");
    }
}
