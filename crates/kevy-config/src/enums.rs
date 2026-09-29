//! Redis-compatible config enums (`appendfsync` / `maxmemory-policy` /
//! log level / log sink) with their canonical-name `as_str` / `parse`
//! pairs. Split out of `schema.rs` to keep it under the 500-LOC house
//! cap; every variant and name is verbatim from before the move.

use std::path::PathBuf;

// ───────────── enums ─────────────

/// AOF fsync policy. Matches Redis `appendfsync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum AppendFsync {
    /// `fsync` after every write command. Zero data-loss but ~50% throughput.
    Always,
    /// Background `fsync` about every second. A power loss loses about 1s
    /// plus the time one fsync takes. Default.
    #[default]
    EverySec,
    /// No explicit `fsync`; the OS decides when data reaches the disk. A
    /// power loss can lose ~30s.
    No,
}

impl AppendFsync {
    /// Canonical Redis-compatible name (`always` / `everysec` / `no`).
    /// Used by `CONFIG GET appendfsync` and `CONFIG REWRITE`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::EverySec => "everysec",
            Self::No => "no",
        }
    }
    /// Inverse of [`Self::as_str`] — case-insensitive. `None` for any
    /// other input; used by both the TOML parser and `CONFIG SET`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "always" => Some(Self::Always),
            "everysec" => Some(Self::EverySec),
            "no" => Some(Self::No),
            _ => None,
        }
    }
}

/// Maxmemory eviction policy. 8 variants matching Redis. `NoEviction`
/// (default) returns an error on writes once `maxmemory` is hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum EvictionPolicy {
    /// Refuse writes once `maxmemory` is hit. Default.
    #[default]
    NoEviction,
    /// Approximated LRU across all keys.
    AllKeysLru,
    /// Approximated LFU across all keys.
    AllKeysLfu,
    /// Random key across all keys.
    AllKeysRandom,
    /// Approximated LRU across keys with a TTL.
    VolatileLru,
    /// Approximated LFU across keys with a TTL.
    VolatileLfu,
    /// Random key from those with a TTL.
    VolatileRandom,
    /// Key with the shortest remaining TTL.
    VolatileTtl,
}

impl EvictionPolicy {
    /// Canonical Redis-compatible name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoEviction => "noeviction",
            Self::AllKeysLru => "allkeys-lru",
            Self::AllKeysLfu => "allkeys-lfu",
            Self::AllKeysRandom => "allkeys-random",
            Self::VolatileLru => "volatile-lru",
            Self::VolatileLfu => "volatile-lfu",
            Self::VolatileRandom => "volatile-random",
            Self::VolatileTtl => "volatile-ttl",
        }
    }
    /// Inverse of [`Self::as_str`] — case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "noeviction" => Some(Self::NoEviction),
            "allkeys-lru" => Some(Self::AllKeysLru),
            "allkeys-lfu" => Some(Self::AllKeysLfu),
            "allkeys-random" => Some(Self::AllKeysRandom),
            "volatile-lru" => Some(Self::VolatileLru),
            "volatile-lfu" => Some(Self::VolatileLfu),
            "volatile-random" => Some(Self::VolatileRandom),
            "volatile-ttl" => Some(Self::VolatileTtl),
            _ => None,
        }
    }
}

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
