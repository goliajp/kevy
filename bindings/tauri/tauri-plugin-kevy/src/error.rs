//! The plugin's error type.
//!
//! Tauri requires a command's error to be `Serialize` (it is delivered to the
//! webview as the rejected-promise value). We keep the kevy error taxonomy
//! visible across the IPC boundary: every variant serialises to
//! `{ "kind": "<variant>", "message": "<text>" }` so guest code can branch on
//! `kind` exactly like the native `KevyError` variants (see
//! `docs/client-contract.md` §2).

use kevy_embedded::{KevyError, StoreError};
use serde::{Serialize, Serializer};

/// A plugin-command error, mirroring [`KevyError`] across the IPC boundary.
#[derive(Debug)]
pub enum Error {
    /// A structured store-semantic error (wrong type, non-integer, …).
    Store(StoreError),
    /// An OS / transport failure (persistence I/O).
    Io(String),
    /// A RESP-level or reply-shape failure.
    Protocol(String),
    /// Write rejected: the store is a read-only replica.
    ReadOnly,
    /// A bad argument, rejected before touching state.
    InvalidInput(String),
    /// A named object (index, view, key) does not exist.
    NotFound(String),
    /// The operation is not available on the embedded backend.
    Unsupported(String),
    /// A bounded blocking call ran out its timeout.
    TimedOut,
    /// The in-process pub/sub bus is gone.
    Closed,
}

/// A command result carrying this plugin's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The stable machine-readable variant tag, matching `KevyError`.
    fn kind(&self) -> &'static str {
        match self {
            Error::Store(_) => "Store",
            Error::Io(_) => "Io",
            Error::Protocol(_) => "Protocol",
            Error::ReadOnly => "ReadOnly",
            Error::InvalidInput(_) => "InvalidInput",
            Error::NotFound(_) => "NotFound",
            Error::Unsupported(_) => "Unsupported",
            Error::TimedOut => "TimedOut",
            Error::Closed => "Closed",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Store(e) => write!(f, "store error: {e:?}"),
            Error::Io(m) | Error::Protocol(m) | Error::InvalidInput(m) => write!(f, "{m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::ReadOnly => write!(f, "READONLY the store is a read-only replica"),
            Error::TimedOut => write!(f, "timed out"),
            Error::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<KevyError> for Error {
    fn from(e: KevyError) -> Self {
        match e {
            KevyError::Store(s) => Error::Store(s),
            KevyError::Io(io) => Error::Io(io.to_string()),
            KevyError::Protocol(m) => Error::Protocol(m),
            KevyError::ReadOnly => Error::ReadOnly,
            KevyError::InvalidInput(m) => Error::InvalidInput(m),
            KevyError::NotFound(m) => Error::NotFound(m),
            KevyError::Unsupported(m) => Error::Unsupported(m),
            KevyError::TimedOut => Error::TimedOut,
            KevyError::Closed => Error::Closed,
        }
    }
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let store_variant = match self {
            Error::Store(e) => Some(format!("{e:?}")),
            _ => None,
        };
        let mut st = s.serialize_struct("KevyError", 3)?;
        st.serialize_field("kind", self.kind())?;
        st.serialize_field("message", &self.to_string())?;
        // For `Store(..)` also surface the structured sub-variant (WrongType,
        // NotInteger, …) so guest code can distinguish them without parsing text.
        st.serialize_field("store", &store_variant)?;
        st.end()
    }
}
