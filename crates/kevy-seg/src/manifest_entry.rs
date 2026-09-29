//! One record of the segment manifest: which file is live and the
//! footer summary mirrored from it.

use crate::SegMeta;

/// One live segment as the manifest knows it.
///
/// Built with [`ManifestEntry::new`] from the footer summary
/// [`SegBuilder::finish`](crate::SegBuilder::finish) returns, so the
/// mirrored range and count always come from the segment itself.
///
/// # Examples
///
/// ```
/// use kevy_seg::{Manifest, ManifestEntry, SegBuilder};
/// # let dir = kevy_tmpdir::TmpDir::new("manifest-entry");
/// # let dir = dir.path();
/// let mut b = SegBuilder::create(&dir.join("s1.seg"))?;
/// b.push(b"a", b"1")?;
/// b.push(b"z", b"2")?;
/// let mut m = Manifest::open(dir)?;
/// m.add(ManifestEntry::new("s1.seg", b.finish()?))?;
/// // min/max are mirrored from the footer so a reader can skip a
/// // segment without opening it.
/// let first = m.live().next().unwrap();
/// assert_eq!(first.file, "s1.seg");
/// assert_eq!(first.max_key, b"z".to_vec());
/// # Ok::<(), kevy_seg::SegError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ManifestEntry {
    /// Segment file name (relative to the manifest's directory).
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry, Seg, SegBuilder};
    /// # let dir = kevy_tmpdir::TmpDir::new("manifest-entry-file");
    /// # let dir = dir.path();
    /// let mut b = SegBuilder::create(&dir.join("s1.seg"))?;
    /// b.push(b"k", b"v")?;
    /// Manifest::open(dir)?.add(ManifestEntry::new("s1.seg", b.finish()?))?;
    ///
    /// // a reopened ledger names the file; join it to the directory to read it
    /// let m = Manifest::open(dir)?;
    /// let e = m.live().next().unwrap();
    /// assert_eq!(e.file, "s1.seg");
    /// assert_eq!(Seg::open(&dir.join(&e.file))?.get(b"k")?, Some(b"v".to_vec()));
    /// # Ok::<(), kevy_seg::SegError>(())
    /// ```
    pub file: String,
    /// Caller-opaque metadata (table id, bucket, …).
    ///
    /// It survives a reopen byte for byte:
    ///
    /// ```
    /// use kevy_seg::{Manifest, ManifestEntry, SegMeta};
    /// # let dir = kevy_tmpdir::TmpDir::new("manifest-entry-meta");
    /// let e = ManifestEntry::new("s1.seg", SegMeta::default()).with_meta(b"table:9".to_vec());
    /// Manifest::open(dir.path())?.add(e)?;
    /// let m = Manifest::open(dir.path())?;
    /// assert_eq!(m.live().next().unwrap().meta, b"table:9");
    /// # Ok::<(), kevy_seg::SegError>(())
    /// ```
    pub meta: Vec<u8>,
    /// Smallest key in the segment, mirrored from its footer so a
    /// directory can be served without opening the file.
    ///
    /// ```
    /// use kevy_seg::{ManifestEntry, SegBuilder};
    /// # let dir = kevy_tmpdir::TmpDir::new("manifest-entry-min");
    /// let mut b = SegBuilder::create(&dir.path().join("s1.seg"))?;
    /// b.push(b"m", b"1")?;
    /// b.push(b"q", b"2")?;
    /// let e = ManifestEntry::new("s1.seg", b.finish()?);
    /// assert_eq!(e.min_key, b"m");
    /// # Ok::<(), kevy_seg::SegError>(())
    /// ```
    pub min_key: Vec<u8>,
    /// Largest key in the segment. With `min_key` this is the range a
    /// reader tests before deciding the segment is worth opening.
    ///
    /// ```
    /// use kevy_seg::{ManifestEntry, SegBuilder};
    /// # let dir = kevy_tmpdir::TmpDir::new("manifest-entry-max");
    /// let mut b = SegBuilder::create(&dir.path().join("s1.seg"))?;
    /// b.push(b"m", b"1")?;
    /// b.push(b"q", b"2")?;
    /// let e = ManifestEntry::new("s1.seg", b.finish()?);
    /// // a lookup for "z" can skip this segment without opening it
    /// let key: &[u8] = b"z";
    /// assert!(key > e.max_key.as_slice());
    /// # Ok::<(), kevy_seg::SegError>(())
    /// ```
    pub max_key: Vec<u8>,
    /// Record count, mirrored from the footer. Includes tombstones, so
    /// it bounds a scan rather than predicting what it yields.
    ///
    /// ```
    /// use kevy_seg::{ManifestEntry, SegBuilder};
    /// # let dir = kevy_tmpdir::TmpDir::new("manifest-entry-records");
    /// let mut b = SegBuilder::create(&dir.path().join("s1.seg"))?;
    /// b.push(b"a", b"1")?;
    /// b.push(b"b", b"")?;
    /// let e = ManifestEntry::new("s1.seg", b.finish()?);
    /// assert_eq!(e.records, 2);
    /// # Ok::<(), kevy_seg::SegError>(())
    /// ```
    pub records: u64,
}

impl ManifestEntry {
    /// An entry for `file`, mirroring the footer summary of the segment
    /// it names. The caller-opaque `meta` starts empty.
    ///
    /// ```
    /// use kevy_seg::{ManifestEntry, SegMeta};
    /// let e = ManifestEntry::new("s1.seg", SegMeta::default());
    /// assert_eq!((e.file.as_str(), e.records, e.meta.len()), ("s1.seg", 0, 0));
    /// ```
    pub fn new(file: impl Into<String>, seg: SegMeta) -> Self {
        ManifestEntry {
            file: file.into(),
            meta: Vec::new(),
            min_key: seg.min_key,
            max_key: seg.max_key,
            records: seg.records,
        }
    }

    /// Attach caller-opaque metadata (table id, bucket, …).
    ///
    /// ```
    /// use kevy_seg::{ManifestEntry, SegMeta};
    /// let e = ManifestEntry::new("s1.seg", SegMeta::default()).with_meta(b"table:9".to_vec());
    /// assert_eq!(e.meta, b"table:9");
    /// ```
    #[must_use]
    pub fn with_meta(mut self, meta: Vec<u8>) -> Self {
        self.meta = meta;
        self
    }
}
