//! The primitives of `Noise_*_25519_ChaChaPoly_BLAKE2s`, in pure Rust with
//! no dependencies: ChaCha20-Poly1305 (RFC 8439), X25519 (RFC 7748),
//! BLAKE2s (RFC 7693), and HMAC/HKDF over BLAKE2s as the Noise Protocol
//! Framework defines them.
//!
//! No branch and no memory index depends on a key, a scalar or a tag. That
//! is how the code is written, and statistical timing checks on x86_64 and
//! aarch64 found no dependence; a compiler does not promise it, and the
//! crate has not been audited by a third party.
//!
//! ```
//! use kevy_crypto::{aead, blake2s, x25519};
//!
//! // two parties agree on a key...
//! let (a, b) = ([3u8; 32], [5u8; 32]);
//! let shared = x25519::x25519(&a, &x25519::x25519(&b, &x25519::BASEPOINT));
//! let key = blake2s::hash(&shared);
//!
//! // ...and use it to seal a message
//! let mut msg = *b"attack at dawn";
//! let tag = aead::seal(&key, &[0; 12], b"", &mut msg);
//! aead::open(&key, &[0; 12], b"", &mut msg, &tag).unwrap();
//! assert_eq!(&msg, b"attack at dawn");
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod aead;
pub mod blake2s;
mod chacha20;
mod fe25519;
pub mod hkdf;
mod poly1305;
pub mod x25519;

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<aead::AuthError>();
    send_sync::<blake2s::Blake2s>();
};

#[cfg(test)]
mod tests;

/// Equality of two byte strings in time that depends only on their
/// lengths, never on where they first differ.
///
/// ```
/// assert!(kevy_crypto::ct_eq(b"tag", b"tag"));
/// assert!(!kevy_crypto::ct_eq(b"tag", b"taG"));
/// ```
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    core::hint::black_box(diff) == 0
}
