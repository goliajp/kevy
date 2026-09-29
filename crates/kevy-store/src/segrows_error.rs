//! Why a row-segment operation failed.

use std::fmt;
use std::path::PathBuf;

/// Why enabling, sealing into, or stitching from row segments failed.
///
/// ```
/// use kevy_store::SegRowsError;
///
/// let e = SegRowsError::NonUtf8Name;
/// assert_eq!(e.to_string(), "SEGMENTED frame names a non-utf8 segment");
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum SegRowsError {
    /// The segment directory could not be created.
    Io(std::io::Error),
    /// The manifest, or a segment being written, refused.
    Seg(kevy_seg::SegError),
    /// A manifest-registered row segment whose name carries no sequence
    /// number.
    NoSeq {
        /// The segment's file name.
        file: String,
    },
    /// A manifest-registered row segment that would not open.
    Open {
        /// The segment's file name.
        file: String,
        /// Why it would not open.
        source: kevy_seg::SegError,
    },
    /// The segment just written would not reopen.
    Reopen {
        /// The segment's file name.
        file: String,
        /// Why it would not reopen.
        source: kevy_seg::SegError,
    },
    /// A `SEGMENTED` frame whose segment name is not UTF-8.
    NonUtf8Name,
    /// A `SEGMENTED` frame naming a segment the manifest does not list:
    /// the segment set was damaged after the eviction.
    NotInManifest {
        /// The segment the frame names.
        file: String,
        /// The segment directory whose manifest was read.
        dir: PathBuf,
    },
    /// A record in a stitched segment that does not decode.
    Record {
        /// The segment's file name.
        file: String,
        /// What was wrong with the record.
        reason: &'static str,
    },
}

impl fmt::Display for SegRowsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Seg(e) => write!(f, "{e}"),
            Self::NoSeq { file } => write!(f, "row segment '{file}' has no parsable seq"),
            Self::Open { file, source } => write!(f, "open {file}: {source}"),
            Self::Reopen { file, source } => write!(f, "reopen {file}: {source}"),
            Self::NonUtf8Name => f.write_str("SEGMENTED frame names a non-utf8 segment"),
            Self::NotInManifest { file, dir } => write!(
                f,
                "AOF says segment '{file}' holds evicted rows, but the manifest at {} does not \
                 list it — the segment truth set was damaged after the eviction; restore the \
                 segment directory from backup before starting",
                dir.display()
            ),
            Self::Record { file, reason } => write!(f, "segment '{file}': {reason}"),
        }
    }
}

impl std::error::Error for SegRowsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Seg(e) | Self::Open { source: e, .. } | Self::Reopen { source: e, .. } => Some(e),
            _ => None,
        }
    }
}

impl From<kevy_seg::SegError> for SegRowsError {
    fn from(e: kevy_seg::SegError) -> Self {
        Self::Seg(e)
    }
}
