//! Why a cold (spilled) segment operation failed.

use std::fmt;

/// Why sealing entries into a cold segment, or reading them back, failed.
/// The spill is derived data: the caller reports the error and the entries
/// stay rebuildable from the rows.
///
/// ```
/// use kevy_window::ColdError;
///
/// assert_eq!(ColdError::CorruptKey.to_string(), "corrupt cold key");
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum ColdError {
    /// The segment directory could not be created.
    Io(std::io::Error),
    /// A segment or the manifest refused.
    Seg(kevy_seg::SegError),
    /// The segment just written would not reopen.
    Reopen {
        /// The segment's file name.
        file: String,
        /// Why it would not reopen.
        source: kevy_seg::SegError,
    },
    /// A cold key that does not decode.
    CorruptKey,
    /// A cold payload that does not decode.
    CorruptPayload,
}

impl fmt::Display for ColdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Seg(e) => write!(f, "{e}"),
            Self::Reopen { file, source } => write!(f, "reopen {file}: {source}"),
            Self::CorruptKey => f.write_str("corrupt cold key"),
            Self::CorruptPayload => f.write_str("corrupt cold payload"),
        }
    }
}

impl std::error::Error for ColdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Seg(e) | Self::Reopen { source: e, .. } => Some(e),
            Self::CorruptKey | Self::CorruptPayload => None,
        }
    }
}

impl From<kevy_seg::SegError> for ColdError {
    fn from(e: kevy_seg::SegError) -> Self {
        Self::Seg(e)
    }
}
