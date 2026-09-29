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
    Always,
    /// fsync about once per second; each [`crate::Aof::tick`] (or
    /// [`crate::Aof::maybe_sync`]) writes the buffer into the kernel. A
    /// power loss loses about one second plus the time one fsync takes.
    /// The default.
    #[default]
    EverySec,
    /// Never fsync explicitly: each [`crate::Aof::tick`] writes the buffer
    /// into the kernel, and the OS decides when it reaches the disk.
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
    #[default]
    Strict,
    /// Hop over a corrupt region to the next record whose checksum holds
    /// and keep replaying; the open leaves interior corrupt regions in
    /// place and quarantines only the bytes after the last recoverable
    /// record. v1 logs have no checksums to anchor on and replay strictly.
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
    #[default]
    Print,
    /// Suppress them — for a host that receives the same numbers through
    /// a metric sink, where the line would be a duplicate.
    Quiet,
}
