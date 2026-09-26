//! The Noise IK handshake (`Noise_IK_25519_ChaChaPoly_BLAKE2s`) and the
//! transport it establishes, without I/O.
//!
//! The initiator knows the responder's static public key in advance; the
//! responder learns the initiator's static key from the first message and
//! can decide there whether to accept it. One round trip, then each side
//! holds a [`Transport`]. Messages travel length-prefixed ([`frame`],
//! [`Frames`]) on whatever stream the caller owns.
//!
//! Ephemeral keys are passed in rather than drawn here, so the crate never
//! touches an entropy source: every handshake needs a fresh one from the
//! operating system.
//!
//! ```
//! use kevy_noise::{Initiator, Keypair, Responder};
//!
//! let server = Keypair::from_secret([1; 32]);
//! let client = Keypair::from_secret([2; 32]);
//!
//! let (m1, init) =
//!     Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"kevy", b"").unwrap();
//! let (_, resp) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"kevy", &m1).unwrap();
//! assert_eq!(resp.remote_static(), client.public());
//! let (m2, mut server_t) = resp.finish(b"").unwrap();
//! let (_, mut client_t) = init.finish(&m2).unwrap();
//!
//! let msg = client_t.seal(b"SET k v").unwrap();
//! assert_eq!(server_t.open(&msg).unwrap(), b"SET k v");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod handshake;
mod symmetric;
mod transport;

#[cfg(test)]
mod tests;

pub use handshake::{Initiator, Keypair, Responder};
pub use transport::{Frames, MAX_MESSAGE, Opener, Sealer, Transport, frame};

/// Why a handshake or transport message was refused.
///
/// ```
/// assert_eq!(kevy_noise::Error::Decrypt.to_string(), "message failed authentication");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A message failed authentication: altered, replayed, out of order, or
    /// from someone without the keys.
    ///
    /// ```
    /// # use kevy_noise::{Error, Initiator, Keypair, Responder};
    /// let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// let (m1, _) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"a", b"").unwrap();
    /// let wrong_prologue = Responder::accept(&server, Keypair::from_secret([4; 32]), b"b", &m1);
    /// assert_eq!(wrong_prologue.err(), Some(Error::Decrypt));
    /// ```
    Decrypt,
    /// A message was shorter than its fixed parts.
    ///
    /// ```
    /// # use kevy_noise::{Error, Keypair, Responder};
    /// let r = Responder::accept(&Keypair::from_secret([1; 32]), Keypair::from_secret([4; 32]), b"", &[1; 10]);
    /// assert_eq!(r.err(), Some(Error::Truncated));
    /// ```
    Truncated,
    /// A peer's public key was a point of small order.
    ///
    /// ```
    /// # use kevy_noise::{Error, Keypair, Responder};
    /// let zero_ephemeral = [0u8; 96];
    /// let r = Responder::accept(&Keypair::from_secret([1; 32]), Keypair::from_secret([4; 32]), b"", &zero_ephemeral);
    /// assert_eq!(r.err(), Some(Error::LowOrderPoint));
    /// ```
    LowOrderPoint,
    /// A message is longer than [`MAX_MESSAGE`] allows.
    ///
    /// ```
    /// let big = vec![0u8; kevy_noise::MAX_MESSAGE + 1];
    /// assert_eq!(kevy_noise::frame(&big).err(), Some(kevy_noise::Error::TooLong));
    /// ```
    TooLong,
    /// 2^64 - 1 messages have been sent or received on this key. A session
    /// would have to run for centuries at line rate to see it.
    ///
    /// ```
    /// assert_ne!(kevy_noise::Error::NonceExhausted, kevy_noise::Error::Decrypt);
    /// ```
    NonceExhausted,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Decrypt => "message failed authentication",
            Error::Truncated => "message too short",
            Error::LowOrderPoint => "peer key is a point of small order",
            Error::TooLong => "message longer than a Noise message allows",
            Error::NonceExhausted => "nonce space exhausted",
        })
    }
}

impl std::error::Error for Error {}
