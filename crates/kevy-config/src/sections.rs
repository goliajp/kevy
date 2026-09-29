//! The `[server]`, `[persistence]` and `[memory]` sections.

use std::path::PathBuf;

use crate::schema::{AppendFsync, EvictionPolicy};

/// `[server]` section.
///
/// ```
/// use kevy_config::Config;
///
/// let cfg = Config::from_toml_str("[server]\nbind = \"0.0.0.0\"\nport = 7000\n", None)?;
/// assert_eq!(cfg.server.bind, [0, 0, 0, 0]);
/// assert_eq!(cfg.server.port, 7000);
/// assert_eq!(cfg.server.max_clients, 10_000, "untouched keys keep their defaults");
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ServerSection {
    /// IPv4 bind address. Default `127.0.0.1`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().server.bind, [127, 0, 0, 1]);
    /// let cfg = kevy_config::Config::from_toml_str("[server]\nbind = \"10.0.0.5\"\n", None)?;
    /// assert_eq!(cfg.server.bind, [10, 0, 0, 5]);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub bind: [u8; 4],
    /// TCP port. Default `6004`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().server.port, 6004);
    /// let cfg = kevy_config::Config::from_toml_str("[server]\nport = 6380\n", None)?;
    /// assert_eq!(cfg.server.port, 6380);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub port: u16,
    /// Shard / reactor thread count. `0` = auto (CPU count). Default `0`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().server.threads, 0); // one per CPU
    /// let cfg = kevy_config::Config::from_toml_str("[server]\nthreads = 4\n", None)?;
    /// assert_eq!(cfg.server.threads, 4);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub threads: usize,
    /// Only shards `0..N` arm accept SQE; rest stay compute-only.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().server.accept_shards, None); // every shard accepts
    /// let cfg = kevy_config::Config::from_toml_str("[server]\naccept_shards = 2\n", None)?;
    /// assert_eq!(cfg.server.accept_shards, Some(2));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub accept_shards: Option<usize>,
    /// Store a declared table's rows in the packed representation: the
    /// columns in declared order in one buffer, with no per-row field names
    /// and no per-row hash table.
    ///
    /// Default `true` since 5.4.1. It shipped off in 5.4.0 for three reasons
    /// and each was then measured away:
    ///
    /// - *the adoption path costs memory* — true only of a probe that never
    ///   read a row back. The saving is collected on reads, not writes: a
    ///   query phase adds 359 B/row to the general form and 56 to this one
    ///   (`the-gap-opens-when-the-rows-are-read`);
    /// - *an unexplained sign difference* — that was the same thing;
    /// - *it stops tiering demoting* — at three million rows against a
    ///   512 MB budget it demotes 2,998,956 keys, more than the general form
    ///   (`the-tiering-budget-is-denominated-in-a-number-that-is-not-the-memory`).
    ///
    /// A deployment that wants 5.4.0's representation sets this to `false`;
    /// nothing about the wire or the on-disk formats changes either way.
    ///
    /// ```
    /// assert!(kevy_config::Config::default().server.packed_rows);
    /// let cfg = kevy_config::Config::from_toml_str("[server]\npacked_rows = false\n", None)?;
    /// assert!(!cfg.server.packed_rows);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub packed_rows: bool,
    /// Cap on total active client connections. `0` = unlimited.
    /// Default `10000` (matches Redis). New connection past cap is closed
    /// + `rejected_connections` counter increments + INFO clients reports.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().server.max_clients, 10_000);
    /// let cfg = kevy_config::Config::from_toml_str("[server]\nmax_clients = 0\n", None)?;
    /// assert_eq!(cfg.server.max_clients, 0); // unlimited
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub max_clients: usize,
    /// Snapshot + AOF location. Default `.`.
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// assert_eq!(kevy_config::Config::default().server.data_dir, Path::new("."));
    /// let cfg = kevy_config::Config::from_toml_str("[server]\ndata_dir = \"/var/lib/kevy\"\n", None)?;
    /// assert_eq!(cfg.server.data_dir, Path::new("/var/lib/kevy"));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub data_dir: PathBuf,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            bind: [127, 0, 0, 1],
            port: 6004,
            threads: 0,
            accept_shards: None,
            packed_rows: true,
            max_clients: 10_000,
            data_dir: PathBuf::from("."),
        }
    }
}

/// `[persistence]` section.
///
/// ```
/// use kevy_config::{AppendFsync, Config};
///
/// let cfg = Config::from_toml_str("[persistence]\nappendfsync = \"no\"\n", None)?;
/// assert!(cfg.persistence.aof);
/// assert_eq!(cfg.persistence.appendfsync, AppendFsync::No);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PersistenceSection {
    /// Append-only file enabled. Default `true`.
    ///
    /// ```
    /// assert!(kevy_config::Config::default().persistence.aof);
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\naof = false\n", None)?;
    /// assert!(!cfg.persistence.aof);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub aof: bool,
    /// AOF fsync policy. Default `EverySec`.
    ///
    /// ```
    /// use kevy_config::AppendFsync;
    ///
    /// assert_eq!(kevy_config::Config::default().persistence.appendfsync, AppendFsync::EverySec);
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\nappendfsync = \"always\"\n", None)?;
    /// assert_eq!(cfg.persistence.appendfsync, AppendFsync::Always);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub appendfsync: AppendFsync,
    /// Trigger BGREWRITEAOF when current AOF is at least this fraction
    /// (as a percent — 100 = 2× the last-rewrite size) larger than the
    /// last rewrite. Default `100`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().persistence.auto_aof_rewrite_percentage, 100);
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\nauto_aof_rewrite_percentage = 200\n", None)?;
    /// assert_eq!(cfg.persistence.auto_aof_rewrite_percentage, 200); // rewrite at 3x
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub auto_aof_rewrite_percentage: u32,
    /// Never auto-rewrite an AOF smaller than this. Default `64mb` =
    /// `64 * 1024 * 1024`.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\nauto_aof_rewrite_min_size = \"128mb\"\n", None)?;
    /// assert_eq!(cfg.persistence.auto_aof_rewrite_min_size, 128 << 20);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub auto_aof_rewrite_min_size: u64,
    /// Absolute-size auto-rewrite trigger: compact whenever the AOF
    /// reaches this many bytes, regardless of growth ratio. `0` = rule
    /// off (the default). The growth rule alone lets a large log double
    /// before compacting — this caps it outright.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().persistence.auto_aof_rewrite_bytes, 0); // off
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\nauto_aof_rewrite_bytes = \"1gb\"\n", None)?;
    /// assert_eq!(cfg.persistence.auto_aof_rewrite_bytes, 1 << 30);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub auto_aof_rewrite_bytes: u64,
    /// Time-based auto-rewrite trigger: compact at least this often (in
    /// seconds) while the log grows. `0` = rule off (the default).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().persistence.auto_aof_rewrite_interval_secs, 0); // off
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\nauto_aof_rewrite_interval_secs = 3600\n", None)?;
    /// assert_eq!(cfg.persistence.auto_aof_rewrite_interval_secs, 3600);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub auto_aof_rewrite_interval_secs: u64,
    /// Best-effort boot replay: recover the good records behind a corrupt
    /// v2 AOF record instead of dropping them. Default `false` (strict).
    ///
    /// ```
    /// assert!(!kevy_config::Config::default().persistence.replay_resync); // strict
    /// let cfg = kevy_config::Config::from_toml_str("[persistence]\nreplay_resync = true\n", None)?;
    /// assert!(cfg.persistence.replay_resync);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub replay_resync: bool,
}

impl Default for PersistenceSection {
    fn default() -> Self {
        Self {
            aof: true,
            appendfsync: AppendFsync::EverySec,
            auto_aof_rewrite_percentage: 100,
            auto_aof_rewrite_min_size: 64 * 1024 * 1024,
            auto_aof_rewrite_bytes: 0,
            auto_aof_rewrite_interval_secs: 0,
            replay_resync: false,
        }
    }
}

/// `[memory]` section.
///
/// ```
/// use kevy_config::{Config, EvictionPolicy};
///
/// let cfg = Config::from_toml_str(
///     "[memory]\nmaxmemory = \"2gb\"\nmaxmemory_policy = \"allkeys-lru\"\n",
///     None,
/// )?;
/// assert_eq!(cfg.memory.maxmemory, 2 << 30);
/// assert_eq!(cfg.memory.maxmemory_policy, EvictionPolicy::AllKeysLru);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MemorySection {
    /// Soft memory ceiling in bytes. `0` = unlimited. Default `0`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().memory.maxmemory, 0); // unlimited
    /// let cfg = kevy_config::Config::from_toml_str("[memory]\nmaxmemory = \"512mb\"\n", None)?;
    /// assert_eq!(cfg.memory.maxmemory, 512 << 20);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub maxmemory: u64,
    /// Action when `maxmemory` is hit. Default `NoEviction`.
    ///
    /// ```
    /// use kevy_config::EvictionPolicy;
    ///
    /// assert_eq!(kevy_config::Config::default().memory.maxmemory_policy, EvictionPolicy::NoEviction);
    /// let cfg = kevy_config::Config::from_toml_str("[memory]\nmaxmemory_policy = \"volatile-ttl\"\n", None)?;
    /// assert_eq!(cfg.memory.maxmemory_policy, EvictionPolicy::VolatileTtl);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub maxmemory_policy: EvictionPolicy,
}

impl Default for MemorySection {
    fn default() -> Self {
        Self { maxmemory: 0, maxmemory_policy: EvictionPolicy::NoEviction }
    }
}
