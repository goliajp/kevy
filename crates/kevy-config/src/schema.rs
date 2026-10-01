//! kevy `Config` schema, defaults, and error type. Apply-from-parser and
//! value-coercion logic lives in `apply.rs` so this file stays focused on
//! "what the settings ARE".

use std::path::PathBuf;

// ───────────── enums ─────────────
// The four Redis-compatible enums live in `crate::enums` (500-LOC house
// cap); re-exported here so `crate::schema::{AppendFsync, …}` paths keep
// working unchanged.
pub use crate::enums::{AppendFsync, EvictionPolicy, LogLevel, LogOutput};
pub use crate::notify::NotificationFlags;

// The sections live in `crate::sections` (server, persistence, memory)
// and `crate::tuning` (the rest) to stay under the 500-LOC house cap.
pub use crate::sections::{MemorySection, PersistenceSection, ServerSection};
pub use crate::tuning::{
    AdvancedSection, AuditSection, ExpirySection, LogSection, LuaSection, MetricsSection,
    NotificationSection, SlowlogSection,
};

/// Complete kevy config: defaults + per-section overrides loaded from
/// the TOML file + env + CLI.
///
/// Every section is plain data with public fields and a [`Default`];
/// start from [`Config::default`] and assign what differs.
///
/// ```
/// let mut cfg = kevy_config::Config::default();
/// cfg.server.port = 7000;
/// cfg.cluster.enabled = true;
/// assert_eq!(cfg.server.port, 7000);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub struct Config {
    /// `[server]` settings.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[server]\nport = 7000\n", None)?;
    /// assert_eq!(cfg.server.port, 7000);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub server: ServerSection,
    /// `[persistence]` settings.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\naof = false\n", None)?;
    /// assert!(!cfg.persistence.aof);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub persistence: PersistenceSection,
    /// `[memory]` settings.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[memory]\nmaxmemory = \"1gb\"\n", None)?;
    /// assert_eq!(cfg.memory.maxmemory, 1 << 30);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub memory: MemorySection,
    /// `[metrics]` settings (Prometheus /metrics endpoint).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[metrics]\nlisten_port = 9121\n", None)?;
    /// assert_eq!(cfg.metrics.listen_port, 9121);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub metrics: MetricsSection,
    /// `[audit]` settings (append-only ADMIN-command audit).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[audit]\nlog_path = \"audit.log\"\n", None)?;
    /// assert_eq!(cfg.audit.log_path.to_str(), Some("audit.log"));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub audit: AuditSection,
    /// `[expiry]` settings.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[expiry]\nhz = 20\n", None)?;
    /// assert_eq!(cfg.expiry.hz, 20);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub expiry: ExpirySection,
    /// `[log]` settings.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[log]\nlevel = \"error\"\n", None)?;
    /// assert_eq!(cfg.log.level, kevy_config::LogLevel::Error);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub log: LogSection,
    /// `[notification]` settings (keyspace events).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[notification]\nnotify_keyspace_events = \"K$\"\n", None)?;
    /// assert_eq!(cfg.notification.notify_keyspace_events, "K$");
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub notification: NotificationSection,
    /// `[advanced]` settings (reactor tuning knobs).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[advanced]\npark_timeout_ms = 5\n", None)?;
    /// assert_eq!(cfg.advanced.park_timeout_ms, 5);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub advanced: AdvancedSection,
    /// `[slowlog]` settings (slow-command ring buffer).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[slowlog]\nslower_than_micros = 10000\n", None)?;
    /// assert_eq!(cfg.slowlog.slower_than_micros, 10_000);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub slowlog: SlowlogSection,
    /// `[cluster]` settings (single-node cluster mode).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[cluster]\nenabled = true\n", None)?;
    /// assert!(cfg.cluster.enabled);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub cluster: crate::cluster::ClusterSection,
    /// `[lua]` settings — server-side Lua scripting via the
    /// `kevy-lua` bridge.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[lua]\ntime_limit_ms = 250\n", None)?;
    /// assert_eq!(cfg.lua.time_limit_ms, 250);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub lua: LuaSection,
    /// `[replication]` settings — primary/replica streaming.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nrole = \"primary\"\n", None)?;
    /// assert_eq!(cfg.replication.role, kevy_config::ReplicationRole::Primary);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub replication: crate::replication::ReplicationSection,
    /// `[feed]` settings — CDC consumer surface (FEED.*).
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[feed]\nenabled = true\n", None)?;
    /// assert!(cfg.feed.enabled);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub feed: FeedSection,
    /// `[tiering]` settings — the transparent-tiering RAM budget
    /// (capacity arc). No budget = tiering off.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[tiering]\nbudget = \"70%\"\n", None)?;
    /// assert_eq!(cfg.tiering.budget, Some(kevy_config::TierBudgetSpec::Percent(70)));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub tiering: crate::tiering::TieringSection,
    /// `[secure]` — where this node's key for the encrypted links lives.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().secure, kevy_config::SecureSection::default());
    /// ```
    pub secure: crate::secure::SecureSection,
    /// Path the config was loaded from (for `CONFIG REWRITE`). `None` =
    /// loaded from defaults only / from in-memory string.
    ///
    /// ```
    /// use kevy_config::Config;
    /// use std::path::Path;
    ///
    /// assert_eq!(Config::from_toml_str("", None)?.source_path, None);
    /// let cfg = Config::from_toml_str("", Some(Path::new("/etc/kevy/kevy.toml")))?;
    /// assert_eq!(cfg.source_path.as_deref(), Some(Path::new("/etc/kevy/kevy.toml")));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub source_path: Option<PathBuf>,
}

/// `[feed]` — the CDC consumer surface. When enabled every shard
/// keeps a mutation backlog (even with no replicas) and serves
/// `FEED.READ` / `FEED.TAIL` under the `(generation, offset)` cursor
/// contract (docs/cdc.md).
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str(
///     "[feed]\nenabled = true\nfeed_buffer_size = \"16mb\"\n",
///     None,
/// )?;
/// assert!(cfg.feed.enabled);
/// assert_eq!(cfg.feed.feed_buffer_size, 16 << 20);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FeedSection {
    /// Enable the FEED.* surface. Default `false`.
    ///
    /// ```
    /// assert!(!kevy_config::Config::default().feed.enabled);
    /// let cfg = kevy_config::Config::from_toml_str("[feed]\nenabled = true\n", None)?;
    /// assert!(cfg.feed.enabled);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub enabled: bool,
    /// Per-shard backlog byte budget. Default `64mb`; hard cap `1gb`
    /// (bring-up refuses louder budgets — memory formula:
    /// `nshards × feed_buffer_size` upper bound).
    ///
    /// ```
    /// use kevy_config::Config;
    ///
    /// assert_eq!(Config::default().feed.feed_buffer_size, 64 << 20);
    /// let cfg = Config::from_toml_str("[feed]\nfeed_buffer_size = \"256mb\"\n", None)?;
    /// assert_eq!(cfg.feed.feed_buffer_size, 256 << 20);
    /// assert!(Config::from_toml_str("[feed]\nfeed_buffer_size = \"2gb\"\n", None).is_err()); // over the cap
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub feed_buffer_size: u64,
}

impl Default for FeedSection {
    fn default() -> Self {
        Self { enabled: false, feed_buffer_size: 64 * 1024 * 1024 }
    }
}

// `ConfigError` lives in [`crate::error`] — split out so this file
// stays under the 500-LOC house rule. Re-exported below for any caller
// that still does `kevy_config::schema::ConfigError`.
pub use crate::error::ConfigError;
