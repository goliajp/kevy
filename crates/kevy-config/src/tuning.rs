//! The `[metrics]`, `[audit]`, `[expiry]`, `[log]`, `[advanced]`,
//! `[notification]`, `[lua]` and `[slowlog]` sections.

use std::path::PathBuf;

use crate::schema::{LogLevel, LogOutput};

/// `[metrics]` section — Prometheus-format HTTP exposition.
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str("[metrics]\nlisten_port = 9121\n", None)?;
/// assert_eq!(cfg.metrics.listen_port, 9121);
/// assert_ne!(cfg.metrics, kevy_config::MetricsSection::default()); // the endpoint is on
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub struct MetricsSection {
    /// TCP port for the `/metrics` HTTP endpoint. `0` = OFF (default).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().metrics.listen_port, 0); // no endpoint
    /// let cfg = kevy_config::Config::from_toml_str("[metrics]\nlisten_port = 9121\n", None)?;
    /// assert_eq!(cfg.metrics.listen_port, 9121);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub listen_port: u16,
}

/// `[audit]` section — append-only audit log of ADMIN-class
/// commands (`CONFIG SET` / `CONFIG REWRITE` / `DEBUG` / `FLUSHDB` /
/// `FLUSHALL` / `CLIENT KILL` / `SCRIPT FLUSH` etc.).
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str("[audit]\nlog_path = \"audit.log\"\n", None)?;
/// assert_ne!(cfg.audit, kevy_config::AuditSection::default()); // auditing is on
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AuditSection {
    /// Append-only audit log file. Empty string = OFF (default).
    ///
    /// ```
    /// assert!(kevy_config::Config::default().audit.log_path.as_os_str().is_empty()); // off
    /// let cfg = kevy_config::Config::from_toml_str("[audit]\nlog_path = \"/var/log/kevy-audit.log\"\n", None)?;
    /// assert_eq!(cfg.audit.log_path.to_str(), Some("/var/log/kevy-audit.log"));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub log_path: PathBuf,
}

impl Default for AuditSection {
    fn default() -> Self {
        Self { log_path: PathBuf::new() }
    }
}

/// `[expiry]` section. Controls the TTL background reaper.
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str("[expiry]\nhz = 50\nsample = 40\n", None)?;
/// assert_eq!((cfg.expiry.hz, cfg.expiry.sample), (50, 40));
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ExpirySection {
    /// Reaper frequency in Hz. Default `10` (every 100 ms).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().expiry.hz, 10);
    /// let cfg = kevy_config::Config::from_toml_str("[expiry]\nhz = 100\n", None)?;
    /// assert_eq!(cfg.expiry.hz, 100); // a reaper pass every 10 ms
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub hz: u32,
    /// Keys sampled per reaper cycle. Default `20`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().expiry.sample, 20);
    /// let cfg = kevy_config::Config::from_toml_str("[expiry]\nsample = 64\n", None)?;
    /// assert_eq!(cfg.expiry.sample, 64);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub sample: u32,
}

impl Default for ExpirySection {
    fn default() -> Self {
        Self { hz: 10, sample: 20 }
    }
}

/// `[log]` section.
///
/// ```
/// use kevy_config::{Config, LogLevel, LogOutput};
///
/// let cfg = Config::from_toml_str("[log]\nlevel = \"debug\"\noutput = \"stdout\"\n", None)?;
/// assert_eq!(cfg.log.level, LogLevel::Debug);
/// assert_eq!(cfg.log.output, LogOutput::Stdout);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LogSection {
    /// Log verbosity. Default `Info`.
    ///
    /// ```
    /// use kevy_config::LogLevel;
    ///
    /// assert_eq!(kevy_config::Config::default().log.level, LogLevel::Info);
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"warning\"\n", None)?;
    /// assert_eq!(cfg.log.level, LogLevel::Warn);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub level: LogLevel,
    /// Log sink. Default `Stderr`.
    ///
    /// ```
    /// use kevy_config::LogOutput;
    ///
    /// assert_eq!(kevy_config::Config::default().log.output, LogOutput::Stderr);
    /// let cfg = kevy_config::Config::from_toml_str("[log]\noutput = \"/var/log/kevy.log\"\n", None)?;
    /// assert_eq!(cfg.log.output, LogOutput::parse("/var/log/kevy.log"));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub output: LogOutput,
}

impl Default for LogSection {
    fn default() -> Self {
        Self { level: LogLevel::Info, output: LogOutput::Stderr }
    }
}

/// `[advanced]` section — reactor-loop tuning knobs that used to be
/// hardcoded `const`s in `kevy-rt`. Defaults match the previously
/// hardcoded values, so the existing benchmark numbers
/// translate one-to-one. Tune only if you know what you're doing.
///
/// ```
/// let d = kevy_config::AdvancedSection::default();
/// assert_eq!((d.spin_limit, d.park_timeout_ms, d.tick_check_every, d.ring_capacity), (256, 50, 256, 1024));
/// let cfg = kevy_config::Config::from_toml_str("[advanced]\nspin_limit = 0\n", None)?;
/// assert_eq!(cfg.advanced.spin_limit, 0); // park straight away
/// assert_eq!(cfg.advanced.ring_capacity, 1024);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AdvancedSection {
    /// Iterations the per-core reactor spins on `poll(timeout=0)`
    /// before parking on a blocking wait. Higher = lower wake-up
    /// latency under contention, higher idle CPU; lower = the inverse.
    /// Default `256` (matches the original hardcoded const).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().advanced.spin_limit, 256);
    /// let cfg = kevy_config::Config::from_toml_str("[advanced]\nspin_limit = 4096\n", None)?;
    /// assert_eq!(cfg.advanced.spin_limit, 4096);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub spin_limit: u32,
    /// Bounded blocking wait in ms once the reactor parks. Acts as a
    /// safety backstop for any missed cross-core wake (the per-pair
    /// SeqCst fence is the primary mechanism).
    /// Default `50` ms.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().advanced.park_timeout_ms, 50);
    /// let cfg = kevy_config::Config::from_toml_str("[advanced]\npark_timeout_ms = 10\n", None)?;
    /// assert_eq!(cfg.advanced.park_timeout_ms, 10);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub park_timeout_ms: u32,
    /// How many reactor loop iterations between wall-clock reads for
    /// the tick (TTL reaper / auto-AOF-rewrite / live-config refresh).
    /// In busy-poll mode (~1M iter/s) the default `256` is one check
    /// per ~256 µs — plenty for a 10 Hz tick. In park mode the
    /// reactor bypasses this throttle (each iter is already ≥ 1 ms),
    /// so the value only matters under sustained load. Default `256`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().advanced.tick_check_every, 256);
    /// let cfg = kevy_config::Config::from_toml_str("[advanced]\ntick_check_every = 64\n", None)?;
    /// assert_eq!(cfg.advanced.tick_check_every, 64);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub tick_check_every: u32,
    /// Per-direction SPSC ring slot count (one ring per ordered
    /// core-pair). Must be a power of two; the ring code rounds up.
    /// Overflow spills to a local backlog Vec rather than blocking,
    /// so a small ring just shifts work to the slower path. Default
    /// `1024`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().advanced.ring_capacity, 1024);
    /// let cfg = kevy_config::Config::from_toml_str("[advanced]\nring_capacity = 4096\n", None)?;
    /// assert_eq!(cfg.advanced.ring_capacity, 4096);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub ring_capacity: usize,
    /// Buffers in each shard's io_uring receive ring, 16 KiB each: the
    /// ring holds this times 16 KiB per shard, all of it resident once
    /// traffic has cycled through it. A power of two from 1 to 32768 (the
    /// kernel's ceiling). A ring that runs dry costs a re-armed receive,
    /// not an error; raise it for thousands of connections per shard that
    /// all send at once. Linux io_uring reactor only. Default `1024`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().advanced.recv_buffers, 1024);
    /// let cfg = kevy_config::Config::from_toml_str("[advanced]\nrecv_buffers = 4096\n", None)?;
    /// assert_eq!(cfg.advanced.recv_buffers, 4096); // 64 MiB a shard
    /// assert!(kevy_config::Config::from_toml_str("[advanced]\nrecv_buffers = 1000\n", None).is_err());
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub recv_buffers: u32,
}

impl Default for AdvancedSection {
    fn default() -> Self {
        Self {
            spin_limit: 256,
            park_timeout_ms: 50,
            tick_check_every: 256,
            ring_capacity: 1024,
            recv_buffers: 1024,
        }
    }
}

/// `[notification]` section. `notify_keyspace_events` is a string of
/// flag chars (Redis convention): `K` keyspace channel, `E` keyevent
/// channel, `g` generic cmds, `$` string cmds, `l` list, `s` set, `h`
/// hash, `z` zset, `t` stream, `x` expired events, `e` evicted
/// events, `n` new-key events, `A` alias for `g$lshztxe` (every
/// event class except `n`, matching Redis's `A`). Default empty =
/// OFF (Redis default — zero hot-path cost). Any other character is
/// a config error.
///
/// Example: `notify_keyspace_events = "KEA"` enables every event
/// class on BOTH channels. `"K$"` enables only string events on the
/// keyspace channel.
///
/// ```
/// use kevy_config::Config;
///
/// let cfg = Config::from_toml_str("[notification]\nnotify_keyspace_events = \"KEA\"\n", None)?;
/// assert_eq!(cfg.notification.notify_keyspace_events, "KEA");
/// assert!(Config::from_toml_str("[notification]\nnotify_keyspace_events = \"Q\"\n", None).is_err());
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct NotificationSection {
    /// Flag string controlling which keyspace notifications fire. Empty
    /// (default) = OFF: writes pay one atomic load + skip, no publish.
    ///
    /// ```
    /// assert!(kevy_config::Config::default().notification.notify_keyspace_events.is_empty()); // off
    /// let cfg = kevy_config::Config::from_toml_str("[notification]\nnotify_keyspace_events = \"Ex\"\n", None)?;
    /// assert_eq!(cfg.notification.notify_keyspace_events, "Ex"); // expiry events only
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub notify_keyspace_events: String,
}

/// `[lua]` section — Lua scripting limits.
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str(
///     "[lua]\ntime_limit_ms = 1000\nallow_dialects = [\"5.1\"]\n",
///     None,
/// )?;
/// assert_eq!(cfg.lua.time_limit_ms, 1000);
/// assert_eq!(cfg.lua.allow_dialects, ["5.1"]);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LuaSection {
    /// Hard cap on per-`EVAL` Lua execution time in milliseconds.
    /// Matches Redis's `lua-time-limit`. The bridge translates this
    /// to a luna-core instruction budget at VM construction time using
    /// a conservative 40 000-instr/ms estimate (so 5000 ms ≈ 200 M
    /// instructions, the same default that used to be hard-coded).
    /// Set to 0 to disable the cap (unlimited execution).
    /// Default: 5000.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().lua.time_limit_ms, 5000);
    /// let cfg = kevy_config::Config::from_toml_str("[lua]\ntime_limit_ms = 0\n", None)?;
    /// assert_eq!(cfg.lua.time_limit_ms, 0); // no cap
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub time_limit_ms: u64,
    /// Whitelist of allowed Lua dialects. Empty = all five
    /// (5.1/5.2/5.3/5.4/5.5) accepted. Set to `["5.1"]` to lock the
    /// server to pure Redis ecosystem-compat mode and reject any
    /// EVAL whose `#!lua version=N` shebang asks for a newer
    /// dialect. Default: empty (all dialects).
    ///
    /// ```
    /// assert!(kevy_config::Config::default().lua.allow_dialects.is_empty()); // every dialect
    /// let cfg = kevy_config::Config::from_toml_str("[lua]\nallow_dialects = [\"5.1\", \"5.4\"]\n", None)?;
    /// assert_eq!(cfg.lua.allow_dialects, ["5.1", "5.4"]);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub allow_dialects: Vec<String>,
}

impl Default for LuaSection {
    fn default() -> Self {
        Self { time_limit_ms: 5000, allow_dialects: Vec::new() }
    }
}

/// `[slowlog]` section — the per-shard slow-command ring buffer surfaced
/// by `SLOWLOG GET/LEN/RESET`. Default is OFF (`slower_than_micros = -1`)
/// so the hot path never pays the `Instant::now()` pair around dispatch
/// (~30 ns/op, ≈9 % at 3 M ops/s). To enable Redis-style 10 ms tracking,
/// set `slower_than_micros = 10000` in `[slowlog]` or run
/// `CONFIG SET slowlog-log-slower-than 10000`.
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str(
///     "[slowlog]\nslower_than_micros = 10000\nmax_len = 256\n",
///     None,
/// )?;
/// assert_eq!((cfg.slowlog.slower_than_micros, cfg.slowlog.max_len), (10_000, 256));
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SlowlogSection {
    /// Record any command whose execution took at least this many
    /// microseconds (Redis: `< slower_than_micros` is skipped). `-1`
    /// disables the log (zero hot-path cost — no `Instant::now()`
    /// taken); `0` records every command. Default `-1` (OFF).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().slowlog.slower_than_micros, -1); // off
    /// let cfg = kevy_config::Config::from_toml_str("[slowlog]\nslower_than_micros = 0\n", None)?;
    /// assert_eq!(cfg.slowlog.slower_than_micros, 0); // record every command
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub slower_than_micros: i64,
    /// Cap on the per-shard ring buffer. Once exceeded, the oldest
    /// entry is dropped to make room. Across `nshards` shards the
    /// effective server-wide cap is `max_len * nshards`. Default `128`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().slowlog.max_len, 128);
    /// let cfg = kevy_config::Config::from_toml_str("[slowlog]\nmax_len = 1024\n", None)?;
    /// assert_eq!(cfg.slowlog.max_len, 1024);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub max_len: u32,
}

impl Default for SlowlogSection {
    fn default() -> Self {
        Self { slower_than_micros: -1, max_len: 128 }
    }
}
