//! kevy `Config` schema, defaults, and error type. Apply-from-parser and
//! value-coercion logic lives in `apply.rs` so this file stays focused on
//! "what the settings ARE".

use std::path::PathBuf;

// ───────────── enums ─────────────

/// AOF fsync policy. Matches Redis `appendfsync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendFsync {
    /// `fsync` after every write command. Zero data-loss but ~50% throughput.
    Always,
    /// Background `fsync` every second. Lose at most 1s on crash. Default.
    EverySec,
    /// No explicit `fsync`; let OS pagecache flush. Lose ~30s on crash.
    No,
}

/// Maxmemory eviction policy. 8 variants matching Redis. `NoEviction`
/// (default) returns an error on writes once `maxmemory` is hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Refuse writes once `maxmemory` is hit. Default.
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

/// Log verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Very chatty, useful when debugging a kevy internal bug.
    Trace,
    /// Per-command / per-event detail; turn on locally to chase issues.
    Debug,
    /// Default; startup banner, WARNs, errors, key lifecycle events.
    Info,
    /// Only non-fatal warnings (e.g. unprotected bind) and errors.
    Warn,
    /// Only fatal errors.
    Error,
}

/// Where to write log output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOutput {
    /// Write to standard error (default).
    Stderr,
    /// Write to standard output.
    Stdout,
    /// Append to the named file (path resolved relative to cwd at startup).
    File(PathBuf),
}

// ───────────── sections ─────────────

/// `[server]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSection {
    /// IPv4 bind address. Default `127.0.0.1`.
    pub bind: [u8; 4],
    /// TCP port. Default `6004`.
    pub port: u16,
    /// Shard / reactor thread count. `0` = auto (CPU count). Default `0`.
    pub threads: usize,
    /// Snapshot + AOF location. Default `.`.
    pub data_dir: PathBuf,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            bind: [127, 0, 0, 1],
            port: 6004,
            threads: 0,
            data_dir: PathBuf::from("."),
        }
    }
}

/// `[persistence]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistenceSection {
    /// Append-only file enabled. Default `true`.
    pub aof: bool,
    /// AOF fsync policy. Default `EverySec`.
    pub appendfsync: AppendFsync,
    /// Trigger BGREWRITEAOF when current AOF is at least this fraction
    /// (as a percent — 100 = 2× the last-rewrite size) larger than the
    /// last rewrite. Default `100`.
    pub auto_aof_rewrite_percentage: u32,
    /// Never auto-rewrite an AOF smaller than this. Default `64mb` =
    /// `64 * 1024 * 1024`.
    pub auto_aof_rewrite_min_size: u64,
}

impl Default for PersistenceSection {
    fn default() -> Self {
        Self {
            aof: true,
            appendfsync: AppendFsync::EverySec,
            auto_aof_rewrite_percentage: 100,
            auto_aof_rewrite_min_size: 64 * 1024 * 1024,
        }
    }
}

/// `[memory]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySection {
    /// Soft memory ceiling in bytes. `0` = unlimited. Default `0`.
    pub maxmemory: u64,
    /// Action when `maxmemory` is hit. Default `NoEviction`.
    pub maxmemory_policy: EvictionPolicy,
}

impl Default for MemorySection {
    fn default() -> Self {
        Self {
            maxmemory: 0,
            maxmemory_policy: EvictionPolicy::NoEviction,
        }
    }
}

/// `[expiry]` section. Controls the TTL background reaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpirySection {
    /// Reaper frequency in Hz. Default `10` (every 100 ms).
    pub hz: u32,
    /// Keys sampled per reaper cycle. Default `20`.
    pub sample: u32,
}

impl Default for ExpirySection {
    fn default() -> Self {
        Self { hz: 10, sample: 20 }
    }
}

/// `[log]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSection {
    /// Log verbosity. Default `Info`.
    pub level: LogLevel,
    /// Log sink. Default `Stderr`.
    pub output: LogOutput,
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            output: LogOutput::Stderr,
        }
    }
}

// ───────────── top-level Config ─────────────

/// Complete kevy config: defaults + per-section overrides loaded from
/// the TOML file + env + CLI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// `[server]` settings.
    pub server: ServerSection,
    /// `[persistence]` settings.
    pub persistence: PersistenceSection,
    /// `[memory]` settings.
    pub memory: MemorySection,
    /// `[expiry]` settings.
    pub expiry: ExpirySection,
    /// `[log]` settings.
    pub log: LogSection,
    /// Path the config was loaded from (for `CONFIG REWRITE`). `None` =
    /// loaded from defaults only / from in-memory string.
    pub source_path: Option<PathBuf>,
}

// ───────────── error type ─────────────

/// Reasons `Config::load` / `from_toml_str` can fail.
#[derive(Debug)]
pub enum ConfigError {
    /// File could not be opened or read.
    IoOpen {
        /// Path that failed to open.
        path: PathBuf,
        /// Underlying error message.
        err: String,
    },
    /// Tokenizer / parser error with line + column.
    Parse {
        /// 1-based line number in the source.
        line: usize,
        /// 1-based column number in the source.
        col: usize,
        /// Human-readable error.
        msg: String,
    },
    /// Value passed schema validation but the field rejected it
    /// (e.g. unknown enum variant, out-of-range integer).
    Schema {
        /// 1-based line number where the offending value appeared.
        line: usize,
        /// `[section].key` of the rejected setting.
        field: String,
        /// Human-readable error.
        msg: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoOpen { path, err } => {
                write!(f, "kevy-config: cannot read {}: {err}", path.display())
            }
            Self::Parse { line, col, msg } => {
                write!(f, "kevy-config: parse error at line {line} col {col}: {msg}")
            }
            Self::Schema { line, field, msg } => {
                write!(f, "kevy-config: schema error at line {line} on {field}: {msg}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}
