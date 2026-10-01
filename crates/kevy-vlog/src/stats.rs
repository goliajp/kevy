//! The log's size gauges, split from `lib.rs` for the 500-line ceiling.

/// Aggregate gauges for INFO (`vlog_size` / `vlog_dead_bytes` feeders).
///
/// ```
/// use kevy_vlog::Vlog;
/// let dir = kevy_tmpdir::TmpDir::new("vlog-stats-type");
/// let mut v = Vlog::open(dir.path(), 1 << 20)?;
/// let r = v.append(b"k", b"value")?;
/// v.note_dead(r);
/// let s = v.stats();
/// // what INFO derives vlog_dead_bytes from
/// assert_eq!(s.bytes - s.live_bytes, s.bytes);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct VlogStats {
    /// Files in the log, including the one currently being appended to.
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-stats-files");
    /// // a 1-byte threshold rotates before every record but the first
    /// let mut v = Vlog::open(dir.path(), 1)?;
    /// v.append(b"a", b"1")?;
    /// assert_eq!(v.stats().files, 1);
    /// v.append(b"b", b"2")?;
    /// assert_eq!(v.stats().files, 2);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub files: usize,
    /// Bytes on disk across all files — what `vlog_size` reports.
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-stats-bytes");
    /// let mut v = Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"value")?;
    /// // one record: its header plus its body
    /// assert_eq!(v.stats().bytes, r.disk_len() as u64);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub bytes: u64,
    /// Bytes still referenced by a live stub. `bytes - live_bytes` is the
    /// dead fraction compaction exists to reclaim, and is what
    /// `vlog_dead_bytes` reports.
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-stats-live");
    /// let mut v = Vlog::open(dir.path(), 1 << 20)?;
    /// let a = v.append(b"a", b"one")?;
    /// let b = v.append(b"b", b"two")?;
    /// v.note_dead(a);
    /// assert_eq!(v.stats().live_bytes, b.disk_len() as u64);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub live_bytes: u64,
    /// Compaction generation. A `VlogRef` taken before this changed may
    /// have been moved; see `epoch()`.
    ///
    /// ```
    /// use kevy_vlog::{CompactOwner, Vlog, VlogRef};
    /// struct NothingLive;
    /// impl CompactOwner for NothingLive {
    ///     fn is_live(&mut self, _: &[u8], _: VlogRef) -> bool {
    ///         false
    ///     }
    ///     fn moved(&mut self, _: &[u8], _: VlogRef, _: VlogRef) {}
    /// }
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-stats-epoch");
    /// let mut v = Vlog::open(dir.path(), 1)?;
    /// let old = v.append(b"a", b"1")?;
    /// v.append(b"b", b"2")?;
    /// v.note_dead(old);
    /// assert_eq!(v.stats().epoch, 0);
    /// // retiring the sealed first file is one compaction generation
    /// assert_eq!(v.compact_below(100, &mut NothingLive)?, 1);
    /// assert_eq!(v.stats().epoch, 1);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub epoch: u64,
}
