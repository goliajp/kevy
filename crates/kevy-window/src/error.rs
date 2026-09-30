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
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment, WindowShape, WindowSpec};
    /// # let dir = kevy_tmpdir::TmpDir::new("cold-error-doc");
    /// let mut w = kevy_window::WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64);
    /// let mut seg = Segment::new();
    /// seg.apply(b"r:10", None, Some(IndexValue::I64(10)));
    /// seg.apply(b"r:300", None, Some(IndexValue::I64(300)));
    /// let file = dir.path().join("plain-file");
    /// std::fs::write(&file, b"")?;
    /// // a directory cannot be created under a regular file
    /// let e = w.slide(b"t.ts", &mut seg, &file.join("segs")).unwrap_err();
    /// assert!(matches!(e, kevy_window::ColdError::Io(_)));
    /// assert_eq!(seg.stats().entries, 2); // the tree is untouched
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Io(std::io::Error),
    /// A segment or the manifest refused.
    ///
    /// ```
    /// # use kevy_index::{IndexValue, Segment, WindowShape, WindowSpec};
    /// # let dir = kevy_tmpdir::TmpDir::new("cold-error-doc");
    /// let mut w = kevy_window::WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64);
    /// let mut seg = Segment::new();
    /// seg.apply(b"r:10", None, Some(IndexValue::I64(10)));
    /// seg.apply(b"r:300", None, Some(IndexValue::I64(300)));
    /// let file = dir.path().join("plain-file");
    /// std::fs::write(&file, b"")?;
    /// // the segment directory is a regular file, so its manifest will not open
    /// let e = w.slide(b"t.ts", &mut seg, &file).unwrap_err();
    /// assert!(matches!(e, kevy_window::ColdError::Seg(_)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Seg(kevy_seg::SegError),
    /// The segment just written would not reopen.
    ///
    /// ```
    /// use kevy_window::ColdError;
    /// let e = ColdError::Reopen { file: "idx-74-0.seg".into(), source: kevy_seg::SegError::Corrupt("footer") };
    /// assert!(e.to_string().starts_with("reopen idx-74-0.seg: "));
    /// assert!(std::error::Error::source(&e).is_some());
    /// ```
    Reopen {
        /// The segment's file name.
        ///
        /// ```
        /// use kevy_window::ColdError;
        /// let e = ColdError::Reopen { file: "idx-74-0.seg".into(), source: kevy_seg::SegError::Corrupt("footer") };
        /// let ColdError::Reopen { file, .. } = &e else { unreachable!() };
        /// assert_eq!(file, "idx-74-0.seg");
        /// ```
        file: String,
        /// Why it would not reopen.
        ///
        /// ```
        /// use kevy_window::ColdError;
        /// let e = ColdError::Reopen { file: "idx-74-0.seg".into(), source: kevy_seg::SegError::Corrupt("footer") };
        /// let ColdError::Reopen { source, .. } = &e else { unreachable!() };
        /// assert!(matches!(source, kevy_seg::SegError::Corrupt("footer")));
        /// ```
        source: kevy_seg::SegError,
    },
    /// A cold key that does not decode.
    ///
    /// ```
    /// let e = kevy_window::ColdError::CorruptKey;
    /// assert_eq!(e.to_string(), "corrupt cold key");
    /// assert!(std::error::Error::source(&e).is_none());
    /// ```
    CorruptKey,
    /// A cold payload that does not decode.
    ///
    /// ```
    /// let e = kevy_window::ColdError::CorruptPayload;
    /// assert_eq!(e.to_string(), "corrupt cold payload");
    /// ```
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
