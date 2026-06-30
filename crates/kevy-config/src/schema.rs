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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOutput {
    /// Write to standard error (default).
    Stderr,
    /// Write to standard output.
    Stdout,
    /// Append to the named file (path resolved relative to cwd at startup).
    File(PathBuf),
}

impl LogOutput {
    /// Canonical name. `File(p)` renders as the path string.
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Stderr => "stderr".into(),
            Self::Stdout => "stdout".into(),
            Self::File(p) => p.display().to_string().into(),
        }
    }
    /// Inverse of [`Self::as_str`]: `stderr` / `stdout` reserved; any
    /// other string is treated as a file path.
    pub fn parse(s: &str) -> Self {
        match s {
            "stderr" => Self::Stderr,
            "stdout" => Self::Stdout,
            path => Self::File(PathBuf::from(path)),
        }
    }
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
    /// **v1.30** — Only shards `0..N` arm accept SQE; rest stay compute-only.
    pub accept_shards: Option<usize>,
    /// **v1.37** — Cap on total active client connections. `0` = unlimited.
    /// Default `10000` (matches Redis). New connection past cap is closed
    /// + `rejected_connections` counter increments + INFO clients reports.
    pub max_clients: usize,
    /// Snapshot + AOF location. Default `.`.
    pub data_dir: PathBuf,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            bind: [127, 0, 0, 1],
            port: 6004,
            threads: 0,
            accept_shards: None,
            max_clients: 10_000,
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

/// `[metrics]` section — v1.41. Prometheus-format HTTP exposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSection {
    /// TCP port for the `/metrics` HTTP endpoint. `0` = OFF (default).
    pub listen_port: u16,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self { listen_port: 0 }
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

/// `[advanced]` section — reactor-loop tuning knobs that used to be
/// hardcoded `const`s in `kevy-rt`. Defaults match the values shipped
/// in workspace v1.3 / earlier so the existing benchmark numbers
/// translate one-to-one. Tune only if you know what you're doing
/// (`bench/REPORT.md` documents the trade-offs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvancedSection {
    /// Iterations the per-core reactor spins on `poll(timeout=0)`
    /// before parking on a blocking wait. Higher = lower wake-up
    /// latency under contention, higher idle CPU; lower = the inverse.
    /// Default `256` (matches v1.0 const).
    pub spin_limit: u32,
    /// Bounded blocking wait in ms once the reactor parks. Acts as a
    /// safety backstop for any missed cross-core wake (the per-pair
    /// SeqCst fence is the primary mechanism since workspace v1.3.0).
    /// Default `50` ms.
    pub park_timeout_ms: u32,
    /// How many reactor loop iterations between wall-clock reads for
    /// the tick (TTL reaper / auto-AOF-rewrite / live-config refresh).
    /// In busy-poll mode (~1M iter/s) the default `256` is one check
    /// per ~256 µs — plenty for a 10 Hz tick. In park mode the
    /// reactor bypasses this throttle (each iter is already ≥ 1 ms),
    /// so the value only matters under sustained load. Default `256`.
    pub tick_check_every: u32,
    /// Per-direction SPSC ring slot count (one ring per ordered
    /// core-pair). Must be a power of two; the ring code rounds up.
    /// Overflow spills to a local backlog Vec rather than blocking,
    /// so a small ring just shifts work to the slower path. Default
    /// `1024`.
    pub ring_capacity: usize,
}

impl Default for AdvancedSection {
    fn default() -> Self {
        Self {
            spin_limit: 256,
            park_timeout_ms: 50,
            tick_check_every: 256,
            ring_capacity: 1024,
        }
    }
}

/// `[notification]` section. `notify_keyspace_events` is a string of
/// flag chars (Redis convention): `K` keyspace channel, `E` keyevent
/// channel, `g` generic cmds, `$` string cmds, `l` list, `s` set, `h`
/// hash, `z` zset, `A` alias for `g$lshz` (every event class except
/// the not-yet-implemented `x`/`e`/`t`/`n`). Default empty = OFF
/// (Redis default — zero hot-path cost).
///
/// Example: `notify_keyspace_events = "KEA"` enables every event
/// class on BOTH channels. `"K$"` enables only string events on the
/// keyspace channel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationSection {
    /// Flag string controlling which keyspace notifications fire. Empty
    /// (default) = OFF: writes pay one atomic load + skip, no publish.
    pub notify_keyspace_events: String,
}

/// Parsed view of [`NotificationSection::notify_keyspace_events`]. The
/// runtime caches this struct per-shard (hot-reload via the existing
/// `LiveRuntimeConfig` tick path) so the per-write-command check
/// reduces to four bool reads on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NotificationFlags {
    /// `K` — publish on `__keyspace@<db>__:<key>` channel.
    pub keyspace: bool,
    /// `E` — publish on `__keyevent@<db>__:<event>` channel.
    pub keyevent: bool,
    /// `g` — DEL / EXPIRE / PERSIST / RENAME / TYPE / FLUSH etc.
    pub generic: bool,
    /// `$` — SET / GETSET / INCR* / APPEND / MSET / etc.
    pub string: bool,
    /// `l` — LPUSH / RPUSH / LPOP / RPOP / LREM / LSET / LTRIM / …
    pub list: bool,
    /// `s` — SADD / SREM / SPOP / SMOVE / …
    pub set: bool,
    /// `h` — HSET / HDEL / HINCRBY / HSETNX / …
    pub hash: bool,
    /// `z` — ZADD / ZINCRBY / ZREM / ZREMRANGEBY* / …
    pub zset: bool,
    /// `t` — XADD / XDEL / XTRIM / XGROUP / XACK / XCLAIM / XREADGROUP …
    pub stream: bool,
}

impl NotificationFlags {
    /// Notifications are entirely off (no channel enabled OR no class
    /// enabled). The hot-path emits skip via this check before any
    /// further classification or string formatting.
    pub fn is_empty(&self) -> bool {
        !(self.keyspace || self.keyevent)
            || !(self.generic
                || self.string
                || self.list
                || self.set
                || self.hash
                || self.zset
                || self.stream)
    }
}

/// `[slowlog]` section — controls the per-shard slow-command ring
/// buffer surfaced by `SLOWLOG GET/LEN/RESET`. Default is OFF
/// (`slower_than_micros = -1`) so the hot path never pays the
/// `Instant::now()` pair around dispatch (~30 ns/op, ≈9 % at 3 M
/// ops/s). To enable Redis-style 10 ms tracking, set
/// `slower_than_micros = 10000` in `[slowlog]` or run
/// `CONFIG SET slowlog-log-slower-than 10000`.
/// `[lua]` section — v1.27 Lua scripting limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaSection {
    /// Hard cap on per-`EVAL` Lua execution time in milliseconds.
    /// Matches Redis's `lua-time-limit`. The bridge translates this
    /// to a luna instruction budget at VM construction time using a
    /// conservative 40 000-instr/ms estimate (so 5000 ms ≈ 200 M
    /// instructions, the same hard-coded default kevy v1.27 P1-P6
    /// shipped). Set to 0 to disable the cap (unlimited execution).
    /// Default: 5000.
    pub time_limit_ms: u64,
    /// Whitelist of allowed Lua dialects. Empty = all five
    /// (5.1/5.2/5.3/5.4/5.5) accepted. Set to `["5.1"]` to lock the
    /// server to pure Redis ecosystem-compat mode and reject any
    /// EVAL whose `#!lua version=N` shebang asks for a newer
    /// dialect. Default: empty (all dialects).
    pub allow_dialects: Vec<String>,
}

impl Default for LuaSection {
    fn default() -> Self {
        Self {
            time_limit_ms: 5000,
            allow_dialects: Vec::new(),
        }
    }
}

/// `[slowlog]` section — ring buffer of slow commands per shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowlogSection {
    /// Record any command whose execution took at least this many
    /// microseconds (Redis: `< slower_than_micros` is skipped). `-1`
    /// disables the log (zero hot-path cost — no `Instant::now()`
    /// taken); `0` records every command. Default `-1` (OFF).
    pub slower_than_micros: i64,
    /// Cap on the per-shard ring buffer. Once exceeded, the oldest
    /// entry is dropped to make room. Across `nshards` shards the
    /// effective server-wide cap is `max_len * nshards`. Default `128`.
    pub max_len: u32,
}

impl Default for SlowlogSection {
    fn default() -> Self {
        Self {
            slower_than_micros: -1,
            max_len: 128,
        }
    }
}

/// Parse a Redis-style `notify_keyspace_events` flag string into
/// [`NotificationFlags`]. Unknown chars are ignored (forward-compat
/// for `x`/`e`/`t`/`n` not yet implemented — see the section docs).
/// The `A` alias enables every event-class flag except channels.
pub fn parse_notification_flags(s: &str) -> NotificationFlags {
    let mut f = NotificationFlags::default();
    for c in s.chars() {
        match c {
            'K' => f.keyspace = true,
            'E' => f.keyevent = true,
            'g' => f.generic = true,
            '$' => f.string = true,
            'l' => f.list = true,
            's' => f.set = true,
            'h' => f.hash = true,
            'z' => f.zset = true,
            't' => f.stream = true,
            'A' => {
                // Alias for "g$lshzxetd" — every implemented event class.
                // Per Redis spec `A` includes the stream `t` class.
                f.generic = true;
                f.string = true;
                f.list = true;
                f.set = true;
                f.hash = true;
                f.zset = true;
                f.stream = true;
            }
            _ => {} // forward-compat: silently ignore unknown chars
        }
    }
    f
}
/// the TOML file + env + CLI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// `[server]` settings.
    pub server: ServerSection,
    /// `[persistence]` settings.
    pub persistence: PersistenceSection,
    /// `[memory]` settings.
    pub memory: MemorySection,
    /// `[metrics]` settings (Prometheus /metrics endpoint — v1.41).
    pub metrics: MetricsSection,
    /// `[expiry]` settings.
    pub expiry: ExpirySection,
    /// `[log]` settings.
    pub log: LogSection,
    /// `[notification]` settings (keyspace events).
    pub notification: NotificationSection,
    /// `[advanced]` settings (reactor tuning knobs).
    pub advanced: AdvancedSection,
    /// `[slowlog]` settings (slow-command ring buffer).
    pub slowlog: SlowlogSection,
    /// `[cluster]` settings (single-node cluster mode).
    pub cluster: crate::cluster::ClusterSection,
    /// `[lua]` settings (v1.27 server-side Lua scripting via the
    /// kevy-lua bridge).
    pub lua: LuaSection,
    /// `[replication]` settings (v3-cluster Phase 1 primary/replica).
    pub replication: crate::replication::ReplicationSection,
    /// Path the config was loaded from (for `CONFIG REWRITE`). `None` =
    /// loaded from defaults only / from in-memory string.
    pub source_path: Option<PathBuf>,
}

// `ConfigError` lives in [`crate::error`] — split out so this file
// stays under the 500-LOC house rule. Re-exported below for any caller
// that still does `kevy_config::schema::ConfigError`.
pub use crate::error::ConfigError;
