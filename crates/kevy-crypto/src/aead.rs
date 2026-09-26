//! ChaCha20-Poly1305 AEAD (RFC 8439 §2.8), in place.
//!
//! ```
//! let mut buf = *b"payload";
//! let tag = kevy_crypto::aead::seal(&[1; 32], &[2; 12], b"aad", &mut buf);
//! assert!(kevy_crypto::aead::open(&[1; 32], &[2; 12], b"aad", &mut buf, &tag).is_ok());
//! ```

use crate::{chacha20, ct_eq, poly1305::Poly1305};

/// The tag did not verify; the buffer was left as it arrived.
///
/// ```
/// use kevy_crypto::aead::{self, AuthError};
///
/// let mut buf = [0u8; 4];
/// let wrong_tag = [0u8; 16];
/// assert_eq!(aead::open(&[1; 32], &[0; 12], b"", &mut buf, &wrong_tag), Err(AuthError));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthError;

impl core::fmt::Display for AuthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("authentication tag mismatch")
    }
}

fn tag(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
    let block0 = chacha20::block(key, 0, nonce);
    let mut otk = [0u8; 32];
    otk.copy_from_slice(&block0[..32]);
    let mut p = Poly1305::new(&otk);
    let zeros = [0u8; 16];
    p.update(aad);
    p.update(&zeros[..(16 - aad.len() % 16) % 16]);
    p.update(ciphertext);
    p.update(&zeros[..(16 - ciphertext.len() % 16) % 16]);
    p.update(&(aad.len() as u64).to_le_bytes());
    p.update(&(ciphertext.len() as u64).to_le_bytes());
    p.finalize()
}

/// Encrypt `buf` in place and return the 16-byte tag over `aad` and the
/// ciphertext.
///
/// A (key, nonce) pair must never be used twice.
///
/// ```
/// use kevy_crypto::aead;
///
/// let key = [7u8; 32];
/// let nonce = [0u8; 12];
/// let mut buf = *b"hello";
/// let tag = aead::seal(&key, &nonce, b"header", &mut buf);
/// assert_ne!(&buf, b"hello");
/// aead::open(&key, &nonce, b"header", &mut buf, &tag).unwrap();
/// assert_eq!(&buf, b"hello");
/// ```
pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; 16] {
    chacha20::xor_keystream(key, 1, nonce, buf);
    tag(key, nonce, aad, buf)
}

/// Verify `tag` over `aad` and the ciphertext in `buf`, then decrypt `buf`
/// in place. On a mismatch nothing is decrypted.
///
/// ```
/// use kevy_crypto::aead::{self, AuthError};
///
/// let key = [7u8; 32];
/// let nonce = [0u8; 12];
/// let mut buf = *b"hello";
/// let tag = aead::seal(&key, &nonce, b"", &mut buf);
/// buf[0] ^= 1;
/// let tampered = buf;
/// assert_eq!(aead::open(&key, &nonce, b"", &mut buf, &tag), Err(AuthError));
/// assert_eq!(buf, tampered);
/// ```
pub fn open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    buf: &mut [u8],
    tag_in: &[u8; 16],
) -> Result<(), AuthError> {
    if !ct_eq(&tag(key, nonce, aad, buf), tag_in) {
        return Err(AuthError);
    }
    chacha20::xor_keystream(key, 1, nonce, buf);
    Ok(())
}
