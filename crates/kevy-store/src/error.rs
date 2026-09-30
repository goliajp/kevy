//! [`KevyError`] — the single error type of the embeddable stack.
//!
//! Defined here because `kevy-store` sits at the bottom of every
//! consumer's dependency graph (kevy-embedded, kevy-client, kevy-persist,
//! kevy-rt and the server all already depend on it) and is the source of
//! the dominant structured error, [`StoreError`]. Hosting the unified
//! type here adds no dependency edge anywhere; the protocol crate
//! (`kevy-resp`) hosting it would require a protocol → store edge.

use crate::StoreError;
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use core::fmt;
#[cfg(feature = "std")]
use std::io;

/// Unified result alias over [`KevyError`].
///
/// ```
/// use kevy_store::{KevyResult, Store};
/// fn counter(s: &mut Store) -> KevyResult<i64> {
///     Ok(s.incr_by(b"hits", 1)?)
/// }
/// let mut s = Store::new();
/// assert_eq!(counter(&mut s)?, 1);
/// # Ok::<(), kevy_store::KevyError>(())
/// ```
pub type KevyResult<T> = core::result::Result<T, KevyError>;

/// The error type of the embeddable stack: `kevy_embedded::Store` and
/// `kevy_client::Connection` surfaces return `Result<_, KevyError>`.
///
/// Structured errors stay structured — a wrong-type write arrives as
/// `KevyError::Store(StoreError::WrongType)`, not as stringly `io::Error`
/// text. `From<io::Error>` / `From<StoreError>` keep `?` ergonomic at
/// both the OS boundary and the store boundary.
///
/// ```
/// use kevy_store::{KevyError, StoreError};
/// let e = KevyError::from(StoreError::WrongType);
/// assert_eq!(e.to_string(), "store error: wrong type for this operation");
/// assert!(std::error::Error::source(&e).is_some());
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum KevyError {
    /// Structured store-semantic error (wrong type, non-integer,
    /// overflow, out-of-memory, …).
    ///
    /// ```
    /// use kevy_store::{KevyError, SetCondition, Store, StoreError};
    /// let mut s = Store::new();
    /// s.set(b"k", b"text".to_vec(), None, SetCondition::Always);
    /// let e = KevyError::from(s.incr_by(b"k", 1).unwrap_err());
    /// assert!(matches!(e, KevyError::Store(StoreError::NotInteger)));
    /// ```
    Store(StoreError),
    /// Operating-system / transport failure (file, socket, AOF).
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let e: KevyError = std::fs::read("/nonexistent/kevy/file").unwrap_err().into();
    /// let KevyError::Io(io) = &e else { panic!("expected Io") };
    /// assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    /// ```
    #[cfg(feature = "std")]
    Io(io::Error),
    /// RESP-level failure on a client link: a server error reply
    /// (`-ERR …` text preserved verbatim) or a malformed / unexpected
    /// reply shape.
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let e = KevyError::Protocol("ERR unknown command".into());
    /// assert_eq!(e.to_string(), "protocol error: ERR unknown command");
    /// ```
    Protocol(String),
    /// Write rejected: the target is a read-only replica.
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let io: std::io::Error = KevyError::ReadOnly.into();
    /// assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    /// ```
    ReadOnly,
    /// Invalid argument to a typed API (bad flag combination, empty
    /// prefix, malformed URL, …). Rejected before touching any state.
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let e = KevyError::InvalidInput("empty prefix".into());
    /// assert_eq!(e.to_string(), "invalid input: empty prefix");
    /// ```
    InvalidInput(String),
    /// A named object (index, view, required key) doesn't exist.
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let io: std::io::Error = KevyError::NotFound("idx:users".into()).into();
    /// assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    /// ```
    NotFound(String),
    /// The operation isn't available on this backend or build.
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let e = KevyError::Unsupported("tiering on wasm".into());
    /// assert_eq!(e.to_string(), "unsupported: tiering on wasm");
    /// ```
    Unsupported(String),
    /// A bounded blocking call ran out its timeout.
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// let io: std::io::Error = KevyError::TimedOut.into();
    /// assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
    /// ```
    TimedOut,
    /// The connection / in-process bus is gone (EOF).
    ///
    /// ```
    /// use kevy_store::KevyError;
    /// assert_eq!(KevyError::Closed.to_string(), "connection closed");
    /// ```
    Closed,
}

impl fmt::Display for KevyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store error: {e}"),
            #[cfg(feature = "std")]
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::ReadOnly => f.write_str("write refused: this is a read-only replica"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::TimedOut => write!(f, "timed out"),
            Self::Closed => write!(f, "connection closed"),
        }
    }
}

impl core::error::Error for KevyError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            #[cfg(feature = "std")]
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for KevyError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

#[cfg(feature = "std")]
impl From<io::Error> for KevyError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The interop a consumer inside an `io::Result` world needs (
/// dogfood F2): kevy provides the conversion — the orphan rule means
/// nobody else can — so `?` works without 280 hand-rolled
/// `io::Error::other` wrappers. Kind-mapped, and **source-preserving**:
/// except for the `Io` passthrough, the original [`KevyError`] rides as
/// the error's source, so a caller that later cares can downcast it
/// back out instead of losing the type at the boundary.
#[cfg(feature = "std")]
impl From<KevyError> for io::Error {
    fn from(e: KevyError) -> Self {
        use io::ErrorKind as K;
        match e {
            KevyError::Io(inner) => inner,
            other => {
                let kind = match &other {
                    KevyError::Io(_) => unreachable!("moved out above"),
                    KevyError::TimedOut => K::TimedOut,
                    KevyError::Closed => K::ConnectionAborted,
                    KevyError::InvalidInput(_) => K::InvalidInput,
                    KevyError::NotFound(_) => K::NotFound,
                    KevyError::Unsupported(_) => K::Unsupported,
                    KevyError::ReadOnly => K::PermissionDenied,
                    KevyError::Protocol(_) => K::InvalidData,
                    KevyError::Store(StoreError::OutOfMemory) => K::OutOfMemory,
                    KevyError::Store(_) => K::InvalidData,
                };
                io::Error::new(kind, other)
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "std")]
mod io_interop_tests {
    use super::*;

    /// The kind survives the boundary and the typed
    /// error rides as source — strictly better than the
    /// `io::Error::other` wrapping it replaces.
    #[test]
    fn kinds_map_and_the_source_is_the_typed_error() {
        let cases: [(KevyError, io::ErrorKind); 6] = [
            (KevyError::TimedOut, io::ErrorKind::TimedOut),
            (KevyError::Closed, io::ErrorKind::ConnectionAborted),
            (KevyError::InvalidInput("x".into()), io::ErrorKind::InvalidInput),
            (KevyError::NotFound("idx".into()), io::ErrorKind::NotFound),
            (KevyError::Store(StoreError::WrongType), io::ErrorKind::InvalidData),
            (KevyError::Store(StoreError::OutOfMemory), io::ErrorKind::OutOfMemory),
        ];
        for (kevy, kind) in cases {
            let msg = kevy.to_string();
            let io: io::Error = kevy.into();
            assert_eq!(io.kind(), kind, "{msg}");
            let src = io.get_ref().expect("source preserved");
            assert!(src.is::<KevyError>(), "downcastable back to KevyError");
            assert_eq!(src.to_string(), msg, "message survives");
        }
    }

    /// An `Io` variant passes through untouched — no double wrap.
    #[test]
    fn io_passes_through_without_wrapping() {
        let inner = io::Error::new(io::ErrorKind::BrokenPipe, "pipe");
        let io: io::Error = KevyError::Io(inner).into();
        assert_eq!(io.kind(), io::ErrorKind::BrokenPipe);
        assert!(io.get_ref().is_some_and(|s| !s.is::<KevyError>()), "not re-wrapped");
    }
}
