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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum LogLevel {
    /// Very chatty, useful when debugging a kevy internal bug.
    Trace,
    /// Per-command / per-event detail; turn on locally to chase issues.
    Debug,
    /// Default; startup banner, WARNs, errors, key lifecycle events.
    #[default]
    Info,
    /// Only non-fatal warnings (e.g. unprotected bind) and errors.
    Warn,
    /// Only fatal errors.
    Error,
}

impl LogLevel {
    /// Canonical name. `Warn` renders as `warning` (Redis convention).
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum LogOutput {
    /// Write to standard error (default).
    #[default]
    Stderr,
    /// Write to standard output.
    Stdout,
    /// Append to the named file (path resolved relative to cwd at startup).
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
    pub fn parse(s: &str) -> Self {
        match s {
            "stderr" => Self::Stderr,
            "stdout" => Self::Stdout,
            path => Self::File(PathBuf::from(path)),
        }
    }
}
