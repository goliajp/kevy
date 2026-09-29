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
    ///
    /// ```
    /// use kevy_store::{SegRowsError, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-io-{}", std::process::id()));
    /// std::fs::write(&dir, b"a file, not a directory")?;
    /// let mut s = Store::new();
    /// s.enable_seg_rows(&dir.join("segs"))?;
    /// s.hset(b"r", &[(b"a".as_slice(), b"1".as_slice())])?;
    /// // the segment directory is created at the first seal, under a file
    /// let err = s.seal_rows_to_seg(b"t", &[b"r".to_vec()]).unwrap_err();
    /// assert!(matches!(err, SegRowsError::Io(_)));
    /// # std::fs::remove_file(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Io(std::io::Error),
    /// The manifest, or a segment being written, refused.
    ///
    /// ```
    /// use kevy_store::{SegRowsError, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-seg-{}", std::process::id()));
    /// std::fs::write(&dir, b"a file, not a directory")?;
    /// // the manifest cannot be opened inside a file
    /// let err = Store::new().enable_seg_rows(&dir).unwrap_err();
    /// assert!(matches!(err, SegRowsError::Seg(_)));
    /// # std::fs::remove_file(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Seg(kevy_seg::SegError),
    /// A manifest-registered row segment whose name carries no sequence
    /// number.
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry, SegMeta};
    /// use kevy_store::{SegRowsError, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-noseq-v-{}", std::process::id()));
    /// # let _ = std::fs::remove_dir_all(&dir);
    /// # std::fs::create_dir_all(&dir)?;
    /// // a row segment registered under a name with no sequence number
    /// let entry = ManifestEntry::new("noseq.seg", SegMeta::default());
    /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
    /// let err = Store::new().enable_seg_rows(&dir).unwrap_err();
    /// assert!(matches!(err, SegRowsError::NoSeq { .. }));
    /// assert_eq!(err.to_string(), "row segment 'noseq.seg' has no parsable seq");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    NoSeq {
        /// The segment's file name.
        ///
        /// ```
        /// use kevy_seg::{Manifest, ManifestEntry, SegMeta};
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-noseq-f-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// # std::fs::create_dir_all(&dir)?;
        /// // a row segment registered under a name with no sequence number
        /// let entry = ManifestEntry::new("noseq.seg", SegMeta::default());
        /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
        /// let err = Store::new().enable_seg_rows(&dir).unwrap_err();
        /// let SegRowsError::NoSeq { file } = err else { panic!("{err}") };
        /// assert_eq!(file, "noseq.seg");
        /// # std::fs::remove_dir_all(&dir)?;
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        file: String,
    },
    /// A manifest-registered row segment that would not open.
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry, SegMeta};
    /// use kevy_store::{SegRowsError, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-open-v-{}", std::process::id()));
    /// # let _ = std::fs::remove_dir_all(&dir);
    /// # std::fs::create_dir_all(&dir)?;
    /// // registered in the manifest, but the file is gone
    /// let entry = ManifestEntry::new("row-t-0.seg", SegMeta::default());
    /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
    /// let err = Store::new().enable_seg_rows(&dir).unwrap_err();
    /// assert!(matches!(err, SegRowsError::Open { .. }));
    /// assert!(err.to_string().starts_with("open row-t-0.seg: "));
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Open {
        /// The segment's file name.
        ///
        /// ```
        /// use kevy_seg::{Manifest, ManifestEntry, SegMeta};
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-open-f-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// # std::fs::create_dir_all(&dir)?;
        /// // registered in the manifest, but the file is gone
        /// let entry = ManifestEntry::new("row-t-0.seg", SegMeta::default());
        /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
        /// let err = Store::new().enable_seg_rows(&dir).unwrap_err();
        /// let SegRowsError::Open { file, .. } = err else { panic!("{err}") };
        /// assert_eq!(file, "row-t-0.seg");
        /// # std::fs::remove_dir_all(&dir)?;
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        file: String,
        /// Why it would not open.
        ///
        /// ```
        /// use kevy_seg::{Manifest, ManifestEntry, SegMeta};
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-open-s-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// # std::fs::create_dir_all(&dir)?;
        /// // registered in the manifest, but the file is gone
        /// let entry = ManifestEntry::new("row-t-0.seg", SegMeta::default());
        /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
        /// let err = Store::new().enable_seg_rows(&dir).unwrap_err();
        /// let SegRowsError::Open { source, .. } = &err else { panic!("{err}") };
        /// assert!(matches!(source, kevy_seg::SegError::Io(_)), "the file does not exist");
        /// assert!(std::error::Error::source(&err).is_some());
        /// # std::fs::remove_dir_all(&dir)?;
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        source: kevy_seg::SegError,
    },
    /// The segment just written would not reopen.
    ///
    /// ```
    /// use kevy_seg::SegError;
    /// use kevy_store::SegRowsError;
    /// // raised when a segment written a moment ago fails to open again
    /// let err = SegRowsError::Reopen { file: "row-74-0.seg".into(), source: SegError::Corrupt("bad footer") };
    /// assert_eq!(err.to_string(), "reopen row-74-0.seg: corrupt segment: bad footer");
    /// ```
    Reopen {
        /// The segment's file name.
        ///
        /// ```
        /// use kevy_seg::SegError;
        /// use kevy_store::SegRowsError;
        /// // raised when a segment written a moment ago fails to open again
        /// let err = SegRowsError::Reopen { file: "row-74-0.seg".into(), source: SegError::Corrupt("bad footer") };
        /// let SegRowsError::Reopen { file, .. } = &err else { unreachable!() };
        /// assert!(err.to_string().contains(file.as_str()));
        /// ```
        file: String,
        /// Why it would not reopen.
        ///
        /// ```
        /// use kevy_seg::SegError;
        /// use kevy_store::SegRowsError;
        /// // raised when a segment written a moment ago fails to open again
        /// let err = SegRowsError::Reopen { file: "row-74-0.seg".into(), source: SegError::Corrupt("bad footer") };
        /// // the cause is chained as the error source
        /// let cause = std::error::Error::source(&err).map(|c| c.to_string());
        /// assert_eq!(cause.as_deref(), Some("corrupt segment: bad footer"));
        /// ```
        source: kevy_seg::SegError,
    },
    /// A `SEGMENTED` frame whose segment name is not UTF-8.
    ///
    /// ```
    /// use kevy_store::{SegRowsError, Store};
    /// let dir = std::env::temp_dir();
    /// let err = Store::new().apply_segmented(&dir, &[0xff, 0xfe]).unwrap_err();
    /// assert!(matches!(err, SegRowsError::NonUtf8Name));
    /// ```
    NonUtf8Name,
    /// A `SEGMENTED` frame naming a segment the manifest does not list:
    /// the segment set was damaged after the eviction.
    ///
    /// ```
    /// use kevy_store::{SegRowsError, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-notin-v-{}", std::process::id()));
    /// # let _ = std::fs::remove_dir_all(&dir);
    /// let err = Store::new().apply_segmented(&dir, b"row-t-0.seg").unwrap_err();
    /// assert!(matches!(err, SegRowsError::NotInManifest { .. }));
    /// assert!(err.to_string().contains("restore the segment directory from backup"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    NotInManifest {
        /// The segment the frame names.
        ///
        /// ```
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-notin-f-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// let err = Store::new().apply_segmented(&dir, b"row-t-0.seg").unwrap_err();
        /// let SegRowsError::NotInManifest { file, .. } = err else { panic!("{err}") };
        /// assert_eq!(file, "row-t-0.seg");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        file: String,
        /// The segment directory whose manifest was read.
        ///
        /// ```
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-notin-d-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// let err = Store::new().apply_segmented(&dir, b"row-t-0.seg").unwrap_err();
        /// let SegRowsError::NotInManifest { dir: read, .. } = err else { panic!("{err}") };
        /// assert_eq!(read, dir);
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        dir: PathBuf,
    },
    /// A record in a stitched segment that does not decode.
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry, SegBuilder};
    /// use kevy_store::{SegRowsError, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-record-v-{}", std::process::id()));
    /// # let _ = std::fs::remove_dir_all(&dir);
    /// # std::fs::create_dir_all(&dir)?;
    /// // a well-formed segment whose one record is not an encoded row
    /// let mut b = SegBuilder::create(dir.join("row-t-0.seg"))?;
    /// b.push(b"k", b"junk")?;
    /// let entry = ManifestEntry::new("row-t-0.seg", b.finish()?);
    /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
    /// let err = Store::new().apply_segmented(&dir, b"row-t-0.seg").unwrap_err();
    /// assert!(matches!(err, SegRowsError::Record { .. }));
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Record {
        /// The segment's file name.
        ///
        /// ```
        /// use kevy_seg::{Manifest, ManifestEntry, SegBuilder};
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-record-f-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// # std::fs::create_dir_all(&dir)?;
        /// // a well-formed segment whose one record is not an encoded row
        /// let mut b = SegBuilder::create(dir.join("row-t-0.seg"))?;
        /// b.push(b"k", b"junk")?;
        /// let entry = ManifestEntry::new("row-t-0.seg", b.finish()?);
        /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
        /// let err = Store::new().apply_segmented(&dir, b"row-t-0.seg").unwrap_err();
        /// let SegRowsError::Record { file, .. } = err else { panic!("{err}") };
        /// assert_eq!(file, "row-t-0.seg");
        /// # std::fs::remove_dir_all(&dir)?;
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        file: String,
        /// What was wrong with the record.
        ///
        /// ```
        /// use kevy_seg::{Manifest, ManifestEntry, SegBuilder};
        /// use kevy_store::{SegRowsError, Store};
        /// # let dir = std::env::temp_dir().join(format!("kevy-doc-segerr-record-r-{}", std::process::id()));
        /// # let _ = std::fs::remove_dir_all(&dir);
        /// # std::fs::create_dir_all(&dir)?;
        /// // a well-formed segment whose one record is not an encoded row
        /// let mut b = SegBuilder::create(dir.join("row-t-0.seg"))?;
        /// b.push(b"k", b"junk")?;
        /// let entry = ManifestEntry::new("row-t-0.seg", b.finish()?);
        /// Manifest::open(&dir)?.add(entry.with_meta(b"rowcold:t".to_vec()))?;
        /// let err = Store::new().apply_segmented(&dir, b"row-t-0.seg").unwrap_err();
        /// let SegRowsError::Record { reason, .. } = &err else { panic!("{err}") };
        /// assert!(!reason.is_empty());
        /// assert!(err.to_string().ends_with(reason));
        /// # std::fs::remove_dir_all(&dir)?;
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
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
