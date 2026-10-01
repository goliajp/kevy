//! What a replay restored and what it could not, as numbers a host can
//! alert on. Split from `replay.rs` for the 500-LOC house rule.

#[cfg(doc)]
use crate::{replay_aof, replay_aof_resync};

/// What one [`replay_aof`] pass restored — and, crucially, what it could
/// NOT: `dropped_bytes` and `corrupt` are the machine-readable form of the
/// WARN line, so a host can turn "the AOF lost bytes at boot" into an
/// alert instead of a needle in stderr (the 3-day silent-loss incident was
/// exactly this signal going unwatched).
///
/// ```
/// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
///
/// let path = std::env::temp_dir().join(format!("report-doc-{}.aof", std::process::id()));
/// let mut aof = Aof::open(&path, Fsync::Always)?;
/// aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
/// drop(aof);
/// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
/// // what a host alerts on: nothing lost at boot
/// assert_eq!((report.commands, report.dropped_bytes, report.corrupt), (1, 0, false));
/// # std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ReplayReport {
    /// Commands re-applied.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-commands-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
    /// assert_eq!(report.commands, 2);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub commands: u64,
    /// Total file size in bytes (before any repair).
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-bytes-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
    /// assert_eq!(report.bytes, std::fs::metadata(&path)?.len());
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub bytes: u64,
    /// Bytes actually replayed (the valid prefix).
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-replayed-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let clean = std::fs::metadata(&path)?.len();
    /// let mut bytes = std::fs::read(&path)?;
    /// bytes.extend_from_slice(b"\x05\x00"); // a torn header
    /// std::fs::write(&path, &bytes)?;
    /// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
    /// assert_eq!(report.replayed_bytes, clean);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub replayed_bytes: u64,
    /// Bytes past the last complete frame — dropped, then quarantined and
    /// truncated by [`crate::Aof::open`]. The zero tail is not among them.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-dropped-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let mut bytes = std::fs::read(&path)?;
    /// bytes.extend_from_slice(b"\x05\x00"); // a torn header
    /// std::fs::write(&path, &bytes)?;
    /// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
    /// assert_eq!(report.dropped_bytes, 2);
    /// assert!(!report.corrupt, "a torn tail is a crash, not corruption");
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub dropped_bytes: u64,
    /// Zeros from the last record to the end of the file: the unused part
    /// of a mapped log's preallocation, cut off without quarantine.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-zero-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let mut bytes = std::fs::read(&path)?;
    /// bytes.resize(bytes.len() + 64, 0); // unused preallocation
    /// std::fs::write(&path, &bytes)?;
    /// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
    /// assert_eq!((report.zero_tail, report.dropped_bytes, report.commands), (64, 0, 2));
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub zero_tail: u64,
    /// True when the stop was a corrupt frame (vs a clean end or a
    /// partial trailing frame).
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-corrupt-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let mut bytes = std::fs::read(&path)?;
    /// *bytes.last_mut().unwrap() ^= 1; // the last record's payload lies
    /// std::fs::write(&path, &bytes)?;
    /// let report = replay_aof_quiet(&path, Default::default(), |_| {})?;
    /// assert!(report.corrupt);
    /// assert_eq!(report.commands, 1);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub corrupt: bool,
    /// Byte ranges resync skipped over ([`replay_aof_resync`] only):
    /// each is a corrupt region between two valid records. Empty under
    /// the strict replay.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, replay_aof_quiet};
    ///
    /// let path = std::env::temp_dir().join(format!("report-resynced-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let mut bytes = std::fs::read(&path)?;
    /// let first = kevy_persist::AOF2_MAGIC.len();
    /// bytes[first + 12] ^= 1; // damage the first record only
    /// std::fs::write(&path, &bytes)?;
    /// let report = kevy_persist::replay_aof_resync(&path, |_| {})?;
    /// assert_eq!(report.commands, 1);
    /// assert_eq!(report.resynced_ranges.len(), 1);
    /// assert_eq!(report.resynced_ranges[0].0, first as u64);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub resynced_ranges: Vec<(u64, u64)>,
}
