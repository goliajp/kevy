//! [`KevyError`] — the single error type of the embeddable stack.
//!
//! Defined here because `kevy-store` sits at the bottom of every
//! consumer's dependency graph (kevy-embedded, kevy-client, kevy-persist,
//! kevy-rt and the server all already depend on it) and is the source of
//! the dominant structured error, [`StoreError`]. Hosting the unified
//! type here adds no dependency edge anywhere; the protocol crate
//! (`kevy-resp`) hosting it would require a protocol → store edge.

use crate::StoreError;
use std::fmt;
use std::io;

/// Unified result alias over [`KevyError`].
pub type KevyResult<T> = std::result::Result<T, KevyError>;

/// The error type of the embeddable stack: `kevy_embedded::Store` and
/// `kevy_client::Connection` surfaces return `Result<_, KevyError>`.
///
/// Structured errors stay structured — a wrong-type write arrives as
/// `KevyError::Store(StoreError::WrongType)`, not as stringly `io::Error`
/// text. `From<io::Error>` / `From<StoreError>` keep `?` ergonomic at
/// both the OS boundary and the store boundary.
#[derive(Debug)]
pub enum KevyError {
    /// Structured store-semantic error (wrong type, non-integer,
    /// overflow, out-of-memory, …).
    Store(StoreError),
    /// Operating-system / transport failure (file, socket, AOF).
    Io(io::Error),
    /// RESP-level failure on a client link: a server error reply
    /// (`-ERR …` text preserved verbatim) or a malformed / unexpected
    /// reply shape.
    Protocol(String),
    /// Write rejected: the target is a read-only replica.
    ReadOnly,
    /// Invalid argument to a typed API (bad flag combination, empty
    /// prefix, malformed URL, …). Rejected before touching any state.
    InvalidInput(String),
    /// A named object (index, view, required key) doesn't exist.
    NotFound(String),
    /// The operation isn't available on this backend or build.
    Unsupported(String),
    /// A bounded blocking call ran out its timeout.
    TimedOut,
    /// The connection / in-process bus is gone (EOF).
    Closed,
}

impl fmt::Display for KevyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store error: {e:?}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::ReadOnly => {
                write!(f, "READONLY You can't write against a read only replica")
            }
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::TimedOut => write!(f, "timed out"),
            Self::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for KevyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
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

impl From<io::Error> for KevyError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
