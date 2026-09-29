//! The choices a caller makes when opening and replaying a log: when to
//! fsync, what to do at a corrupt record, and whether replay reports on
//! stderr.

/// When to fsync the AOF to disk. The three values of Redis's
/// `appendfsync`.
///
/// ```
/// use kevy_persist::Fsync;
///
/// assert_eq!(Fsync::parse("EverySec"), Some(Fsync::EverySec));
/// assert_eq!(Fsync::Always.as_str(), "always");
/// assert_eq!(Fsync::default(), Fsync::EverySec);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Fsync {
    /// fsync after every write — safest, slowest.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync};
    ///
    /// let path = std::env::temp_dir().join(format!("fsync-always-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
    /// // on disk before append returned: no tick, no drop needed
    /// assert_eq!(std::fs::metadata(&path)?.len(), aof.size_bytes());
    /// # drop(aof);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Always,
    /// fsync about once per second; each [`crate::Aof::tick`] (or
    /// [`crate::Aof::maybe_sync`]) writes the buffer into the kernel. A
    /// power loss loses about one second plus the time one fsync takes.
    /// The default.
    ///
    /// ```
    /// use kevy_persist::{Aof, Fsync};
    ///
    /// let path = std::env::temp_dir().join(format!("fsync-everysec-doc-{}.aof", std::process::id()));
    /// let aof = Aof::open(&path, Fsync::default())?;
    /// assert_eq!(aof.fsync_policy(), Fsync::EverySec);
    /// assert_eq!(Fsync::EverySec.as_str(), "everysec");
    /// # drop(aof);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[default]
    EverySec,
    /// Never fsync explicitly: each [`crate::Aof::tick`] writes the buffer
    /// into the kernel, and the OS decides when it reaches the disk.
    ///
    /// ```
    /// use kevy_persist::{Aof, Fsync};
    ///
    /// let path = std::env::temp_dir().join(format!("fsync-no-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// aof.set_fsync(Fsync::parse("no").unwrap_or_default())?; // CONFIG SET appendfsync no
    /// assert_eq!(aof.fsync_policy(), Fsync::No);
    /// # drop(aof);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    No,
}

impl Fsync {
    /// The Redis spelling (`always` / `everysec` / `no`), as `CONFIG GET
    /// appendfsync` reports it.
    ///
    /// ```
    /// assert_eq!(kevy_persist::Fsync::No.as_str(), "no");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::EverySec => "everysec",
            Self::No => "no",
        }
    }

    /// The inverse of [`Self::as_str`], ignoring ASCII case. `None` for
    /// any other input.
    ///
    /// ```
    /// assert_eq!(kevy_persist::Fsync::parse("ALWAYS"), Some(kevy_persist::Fsync::Always));
    /// assert_eq!(kevy_persist::Fsync::parse("sometimes"), None);
    /// ```
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Always, Self::EverySec, Self::No]
            .into_iter()
            .find(|f| f.as_str().eq_ignore_ascii_case(s))
    }
}

/// What a replay (and the open that follows it) does at a corrupt record
/// in the middle of a log.
///
/// ```
/// use kevy_persist::ReplayMode;
///
/// assert_eq!(ReplayMode::default(), ReplayMode::Strict);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ReplayMode {
    /// Stop at the first corrupt record; the open quarantines and
    /// truncates everything from there.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, ReplayMode, ReplaySummary, replay_aof_in_place};
    ///
    /// let path = std::env::temp_dir().join(format!("mode-strict-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
    /// drop(aof);
    /// let mut bytes = std::fs::read(&path)?;
    /// *bytes.last_mut().unwrap() ^= 1; // flip a payload bit
    /// std::fs::write(&path, &bytes)?;
    /// let report = replay_aof_in_place(&path, ReplayMode::Strict, ReplaySummary::Quiet, |_| {})?;
    /// assert!(report.corrupt);
    /// assert_eq!(report.commands, 0);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[default]
    Strict,
    /// Hop over a corrupt region to the next record whose checksum holds
    /// and keep replaying; the open leaves interior corrupt regions in
    /// place and quarantines only the bytes after the last recoverable
    /// record. v1 logs have no checksums to anchor on and replay strictly.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync, ReplayMode, ReplaySummary, replay_aof_in_place};
    ///
    /// let path = std::env::temp_dir().join(format!("mode-resync-doc-{}.aof", std::process::id()));
    /// let mut aof = Aof::open(&path, Fsync::Always)?;
    /// for key in [b"a", b"b"] {
    ///     aof.append(&Argv::from(vec![b"SET".to_vec(), key.to_vec(), b"v".to_vec()]))?;
    /// }
    /// drop(aof);
    /// let mut bytes = std::fs::read(&path)?;
    /// bytes[kevy_persist::AOF2_MAGIC.len() + 12] ^= 1; // damage the first record only
    /// std::fs::write(&path, &bytes)?;
    /// let report = replay_aof_in_place(&path, ReplayMode::Resync, ReplaySummary::Quiet, |_| {})?;
    /// assert!(report.corrupt);
    /// assert_eq!(report.commands, 1, "the record behind the damage still replays");
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Resync,
}

/// Whether a replay prints its informational summary lines on stderr. The
/// corrupt-record WARN prints either way: it is an incident signal, not
/// information.
///
/// ```
/// use kevy_persist::ReplaySummary;
///
/// assert_eq!(ReplaySummary::default(), ReplaySummary::Print);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ReplaySummary {
    /// Print the summary lines.
    ///
    /// ```
    /// use kevy_persist::{ReplayMode, ReplaySummary, replay_aof_in_place};
    ///
    /// let path = std::env::temp_dir().join(format!("summary-print-doc-{}.aof", std::process::id()));
    /// kevy_persist::write_aof_base(&path)?;
    /// // prints `replayed 0 commands …` on stderr as it returns
    /// let report = replay_aof_in_place(&path, ReplayMode::Strict, ReplaySummary::Print, |_| {})?;
    /// assert_eq!(report.commands, 0);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[default]
    Print,
    /// Suppress them — for a host that receives the same numbers through
    /// a metric sink, where the line would be a duplicate.
    ///
    /// ```
    /// use kevy_persist::{ReplayMode, ReplaySummary, replay_aof_in_place};
    ///
    /// let path = std::env::temp_dir().join(format!("summary-quiet-doc-{}.aof", std::process::id()));
    /// kevy_persist::write_aof_base(&path)?;
    /// // the same numbers, handed back instead of printed
    /// let report = replay_aof_in_place(&path, ReplayMode::Strict, ReplaySummary::Quiet, |_| {})?;
    /// assert_eq!((report.commands, report.dropped_bytes), (0, 0));
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Quiet,
}
