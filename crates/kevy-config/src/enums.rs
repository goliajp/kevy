//! Redis-compatible config enums (`appendfsync` / `maxmemory-policy` /
//! log level / log sink) with their canonical-name `as_str` / `parse`
//! pairs. Split out of `schema.rs` to keep it under the 500-LOC house
//! cap; every variant and name is verbatim from before the move.

use std::path::PathBuf;

// ───────────── enums ─────────────

/// AOF fsync policy (Redis `appendfsync`): the log's own type, which
/// enforces it, so the parsed config and the AOF cannot disagree.
pub use kevy_persist::Fsync as AppendFsync;

/// Maxmemory eviction policy: the store's own type, which enforces it, so
/// the parsed config and the keyspace cannot disagree on a policy.
pub use kevy_store::EvictionPolicy;

/// Log verbosity.
///
/// ```
/// use kevy_config::LogLevel;
///
/// assert_eq!(LogLevel::default(), LogLevel::Info);
/// assert_eq!(LogLevel::parse("warn").map(|l| l.as_str()), Some("warning"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum LogLevel {
    /// Very chatty, useful when debugging a kevy internal bug.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"trace\"\n", None)?;
    /// assert_eq!(cfg.log.level, LogLevel::Trace);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    Trace,
    /// Per-command / per-event detail; turn on locally to chase issues.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"debug\"\n", None)?;
    /// assert_eq!(cfg.log.level, LogLevel::Debug);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    Debug,
    /// Default; startup banner, WARNs, errors, key lifecycle events.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"INFO\"\n", None)?;
    /// assert_eq!(cfg.log.level, LogLevel::Info);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    #[default]
    Info,
    /// Only non-fatal warnings (e.g. unprotected bind) and errors.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"warn\"\n", None)?;
    /// assert_eq!(cfg.log.level, LogLevel::Warn);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    Warn,
    /// Only fatal errors.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"error\"\n", None)?;
    /// assert_eq!(cfg.log.level, LogLevel::Error);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    Error,
}

impl LogLevel {
    /// Canonical name. `Warn` renders as `warning` (Redis convention).
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// assert_eq!(LogLevel::Info.as_str(), "info");
    /// assert_eq!(LogLevel::Warn.as_str(), "warning"); // Redis spelling
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warning",
            Self::Error => "error",
        }
    }
    /// Inverse of [`Self::as_str`] — case-insensitive; accepts both
    /// `warn` and `warning` for the Warn level.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// assert_eq!(LogLevel::parse("Warning"), Some(LogLevel::Warn));
    /// assert_eq!(LogLevel::parse("warn"), Some(LogLevel::Warn));
    /// assert_eq!(LogLevel::parse("verbose"), None);
    /// ```
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Where to write log output.
///
/// ```
/// use kevy_config::LogOutput;
///
/// assert_eq!(LogOutput::default(), LogOutput::Stderr);
/// let cfg = kevy_config::Config::from_toml_str("[log]\noutput = \"stdout\"\n", None)?;
/// assert_eq!(cfg.log.output, LogOutput::Stdout);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum LogOutput {
    /// Write to standard error (default).
    ///
    /// ```
    /// assert_eq!(kevy_config::LogOutput::parse("stderr"), kevy_config::LogOutput::Stderr);
    /// ```
    #[default]
    Stderr,
    /// Write to standard output.
    ///
    /// ```
    /// assert_eq!(kevy_config::LogOutput::parse("stdout"), kevy_config::LogOutput::Stdout);
    /// ```
    Stdout,
    /// Append to the named file (path resolved relative to cwd at startup).
    ///
    /// ```
    /// use kevy_config::LogOutput;
    /// use std::path::PathBuf;
    ///
    /// assert_eq!(LogOutput::parse("kevy.log"), LogOutput::File(PathBuf::from("kevy.log")));
    /// ```
    File(PathBuf),
}

impl LogOutput {
    /// Canonical name. `File(p)` renders as the path string, which costs
    /// an allocation — hence `to_`, not `as_`.
    ///
    /// ```
    /// use kevy_config::LogOutput;
    /// assert_eq!(LogOutput::Stderr.to_config_str(), "stderr");
    /// assert_eq!(LogOutput::parse("/var/log/kevy.log").to_config_str(), "/var/log/kevy.log");
    /// ```
    pub fn to_config_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Stderr => "stderr".into(),
            Self::Stdout => "stdout".into(),
            Self::File(p) => p.display().to_string().into(),
        }
    }
    /// Inverse of [`Self::to_config_str`]: `stderr` / `stdout` reserved;
    /// any other string is treated as a file path.
    ///
    /// ```
    /// use kevy_config::LogOutput;
    ///
    /// assert_eq!(LogOutput::parse("stdout"), LogOutput::Stdout);
    /// let file = LogOutput::parse("/var/log/kevy.log");
    /// assert_eq!(file.to_config_str(), "/var/log/kevy.log");
    /// ```
    pub fn parse(s: &str) -> Self {
        match s {
            "stderr" => Self::Stderr,
            "stdout" => Self::Stdout,
            path => Self::File(PathBuf::from(path)),
        }
    }
}
