//! HMAC (RFC 2104) over BLAKE2s, and the HKDF that the Noise Protocol
//! Framework builds from it (§4.3 of the Noise specification, revision 34).
//!
//! ```
//! let [k1, k2, _] = kevy_crypto::hkdf::hkdf(&[0; 32], b"input key material");
//! assert_ne!(k1, k2);
//! ```

use crate::blake2s::Blake2s;

const BLOCK: usize = 64;

/// HMAC-BLAKE2s of the concatenation of `parts` under `key` (at most 64
/// bytes; Noise only ever uses 32).
///
/// ```
/// use kevy_crypto::hkdf::hmac;
///
/// assert_eq!(hmac(b"k", &[b"ab", b"c"]), hmac(b"k", &[b"abc"]));
/// assert_ne!(hmac(b"k", &[b"abc"]), hmac(b"j", &[b"abc"]));
/// ```
///
/// # Panics
///
/// If `key` is longer than 64 bytes.
pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    assert!(key.len() <= BLOCK, "HMAC-BLAKE2s: key longer than one block");
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for (i, k) in key.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }
    let mut inner = Blake2s::new(32);
    inner.update(&ipad);
    for p in parts {
        inner.update(p);
    }
    let mut inner_digest = [0u8; 32];
    inner.finalize(&mut inner_digest);
    let mut outer = Blake2s::new(32);
    outer.update(&opad);
    outer.update(&inner_digest);
    let mut out = [0u8; 32];
    outer.finalize(&mut out);
    out
}

/// Noise's `HKDF(chaining_key, input_key_material, 3)`; callers that need
/// two outputs ignore the third.
///
/// ```
/// let [a, b, c] = kevy_crypto::hkdf::hkdf(&[0u8; 32], b"ikm");
/// assert!(a != b && b != c);
/// ```
pub fn hkdf(chaining_key: &[u8; 32], ikm: &[u8]) -> [[u8; 32]; 3] {
    let temp = hmac(chaining_key, &[ikm]);
    let o1 = hmac(&temp, &[&[1]]);
    let o2 = hmac(&temp, &[&o1, &[2]]);
    let o3 = hmac(&temp, &[&o2, &[3]]);
    [o1, o2, o3]
}
