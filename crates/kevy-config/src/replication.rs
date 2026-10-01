//! `[replication]` section schema — primary/replica streaming
//! replication.

/// `[replication]` section — primary/replica streaming replication.
/// When `role = "standalone"` (default) this whole subsystem is
/// dormant: no listener, no upstream connection, no buffer
/// allocated. `role = "primary"` brings up a TCP listener on
/// `listen_port` that streams every applied mutation to connected
/// replicas. `role = "replica"` connects to `upstream`, full-syncs
/// from a snapshot, then applies live frames.
///
/// Quorum failover is configured separately via the `[cluster]`
/// `node_id` + `peers` keys; see [`crate::cluster::ClusterSection`].
///
/// ```
/// use kevy_config::{Config, ReplicationRole};
///
/// let cfg = Config::from_toml_str(
///     "[replication]\nrole = \"replica\"\nupstream = \"10.0.0.1:16004\"\n",
///     None,
/// )?;
/// assert_eq!(cfg.replication.role, ReplicationRole::Replica);
/// assert_eq!(cfg.replication.upstream.as_deref(), Some("10.0.0.1:16004"));
/// assert!(cfg.replication.replica_read_only);
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ReplicationSection {
    /// Node role. `Standalone` (default) disables the whole subsystem.
    ///
    /// ```
    /// use kevy_config::ReplicationRole;
    ///
    /// assert_eq!(kevy_config::Config::default().replication.role, ReplicationRole::Standalone);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nrole = \"primary\"\n", None)?;
    /// assert_eq!(cfg.replication.role, ReplicationRole::Primary);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub role: ReplicationRole,
    /// `host:port` of the primary, for `role = "replica"`. Ignored when
    /// `role != "replica"`. `None` for replica = config-error at startup.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.upstream, None);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nupstream = \"primary.internal:16004\"\n", None)?;
    /// assert_eq!(cfg.replication.upstream.as_deref(), Some("primary.internal:16004"));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub upstream: Option<String>,
    /// TCP port BASE the primary listens on for incoming replica
    /// connections — shard `i` binds at `listen_port_base + i`, mirroring
    /// the cluster listener pattern. `0` (default) = `server.port +
    /// 10000` as the base, picked at startup. Only meaningful when
    /// `role = "primary"`. Replicas use a shard-aware client that
    /// connects to all `nshards` ports to mirror the full keyspace.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.listen_port_base, 0); // server.port + 10000
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nlisten_port_base = 17000\n", None)?;
    /// assert_eq!(cfg.replication.listen_port_base, 17000); // shard i at 17000 + i
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub listen_port_base: u16,
    /// Bounded ring buffer for recent applied frames (bytes). A replica
    /// that disconnects and reconnects within this backlog window catches
    /// up without a full snapshot. Default `256mb`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.replication_buffer_size, 256 << 20);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nreplication_buffer_size = \"1gb\"\n", None)?;
    /// assert_eq!(cfg.replication.replication_buffer_size, 1 << 30);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub replication_buffer_size: u64,
    /// How long the primary keeps a disconnected replica's slot before
    /// dropping it (and forcing a full snapshot on reconnect). Default
    /// `60_000` (60 s).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.reconnect_window_ms, 60_000);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nreconnect_window_ms = 5000\n", None)?;
    /// assert_eq!(cfg.replication.reconnect_window_ms, 5000);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub reconnect_window_ms: u32,
    /// Primary refuses writes when fewer than this many
    /// replicas have a live ACK within `min_replicas_max_lag_ms`.
    /// `0` (default) disables the check. A lag heuristic, not a
    /// quorum guarantee — the quorum lease is the real
    /// split-brain fence.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.min_replicas_to_write, 0); // no check
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nmin_replicas_to_write = 1\n", None)?;
    /// assert_eq!(cfg.replication.min_replicas_to_write, 1);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub min_replicas_to_write: u32,
    /// Freshness window for `min_replicas_to_write` (ms).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.min_replicas_max_lag_ms, 10_000);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nmin_replicas_max_lag_ms = 2000\n", None)?;
    /// assert_eq!(cfg.replication.min_replicas_max_lag_ms, 2000);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub min_replicas_max_lag_ms: u32,
    /// Bounded staleness: a replica whose last primary
    /// heartbeat is older than this refuses reads with `-STALE`
    /// (client falls back to the primary). `0` = off (default:
    /// reads always answer, whatever the lag).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().replication.replica_max_staleness_ms, 0); // off
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nreplica_max_staleness_ms = 500\n", None)?;
    /// assert_eq!(cfg.replication.replica_max_staleness_ms, 500);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub replica_max_staleness_ms: u32,
    /// Reject client writes while in the replica role
    /// (`-READONLY`). Default `true`, Redis-compatible; the
    /// replication apply path and admin verbs bypass the gate.
    ///
    /// ```
    /// assert!(kevy_config::Config::default().replication.replica_read_only);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nreplica_read_only = false\n", None)?;
    /// assert!(!cfg.replication.replica_read_only);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub replica_read_only: bool,
    /// The upstream is a SINGLE stream on one port (an embedded
    /// writer's `embed_writer_listen_addr` source) rather than a
    /// per-shard port fleet: one routing runner fans frames into local
    /// shards by key hash. Only meaningful when `role = "replica"`.
    ///
    /// ```
    /// assert!(!kevy_config::Config::default().replication.single_source);
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nsingle_source = true\n", None)?;
    /// assert!(cfg.replication.single_source);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub single_source: bool,
    /// Encrypt and authenticate this node's replication links with Noise:
    /// the link to its upstream, and the links its replicas open. Needs
    /// `[secure] private_key_file`.
    ///
    /// ```
    /// assert!(!kevy_config::Config::default().replication.secure);
    /// ```
    pub secure: bool,
    /// The upstream's public key, which a replica checks the primary
    /// against. Required on a secure replica.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str(
    ///     &format!("[replication]\nupstream_key = \"{}\"\n", "cd".repeat(32)),
    ///     None,
    /// )
    /// .unwrap();
    /// assert_eq!(cfg.replication.upstream_key, Some([0xcd; 32]));
    /// ```
    pub upstream_key: Option<[u8; 32]>,
    /// The replicas a secure primary accepts, by public key. Empty: any
    /// replica may connect, and the link is only encrypted.
    ///
    /// ```
    /// assert!(kevy_config::Config::default().replication.replica_keys.is_empty());
    /// ```
    pub replica_keys: Vec<[u8; 32]>,
}

impl Default for ReplicationSection {
    fn default() -> Self {
        Self {
            role: ReplicationRole::Standalone,
            upstream: None,
            listen_port_base: 0,
            replication_buffer_size: 256 * 1024 * 1024,
            reconnect_window_ms: 60_000,
            min_replicas_to_write: 0,
            min_replicas_max_lag_ms: 10_000,
            replica_max_staleness_ms: 0,
            replica_read_only: true,
            single_source: false,
            secure: false,
            upstream_key: None,
            replica_keys: Vec::new(),
        }
    }
}

/// Node role for the `[replication]` subsystem.
///
/// ```
/// use kevy_config::ReplicationRole;
///
/// assert_eq!(ReplicationRole::default(), ReplicationRole::Standalone);
/// assert_eq!(ReplicationRole::parse("Replica"), Some(ReplicationRole::Replica));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub enum ReplicationRole {
    /// Default. Replication subsystem dormant; behaves like pre-v3.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[replication]\nrole = \"standalone\"\n", None)?;
    /// assert_eq!(cfg.replication.role, kevy_config::ReplicationRole::Standalone);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    #[default]
    Standalone,
    /// This node accepts writes and streams mutations to replicas.
    ///
    /// ```
    /// assert_eq!(kevy_config::ReplicationRole::Primary.as_str(), "primary");
    /// ```
    Primary,
    /// This node connects to a primary and mirrors its keyspace read-only.
    ///
    /// ```
    /// assert_eq!(kevy_config::ReplicationRole::Replica.as_str(), "replica");
    /// ```
    Replica,
}

impl ReplicationRole {
    /// Canonical name used by `CONFIG GET replication.role` and TOML.
    ///
    /// ```
    /// use kevy_config::ReplicationRole;
    ///
    /// assert_eq!(ReplicationRole::Standalone.as_str(), "standalone");
    /// assert_eq!(ReplicationRole::parse(ReplicationRole::Primary.as_str()), Some(ReplicationRole::Primary));
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Primary => "primary",
            Self::Replica => "replica",
        }
    }
    /// Inverse of [`Self::as_str`] — case-insensitive.
    ///
    /// ```
    /// use kevy_config::ReplicationRole;
    ///
    /// assert_eq!(ReplicationRole::parse("PRIMARY"), Some(ReplicationRole::Primary));
    /// assert_eq!(ReplicationRole::parse("leader"), None);
    /// ```
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "standalone" => Some(Self::Standalone),
            "primary" => Some(Self::Primary),
            "replica" => Some(Self::Replica),
            _ => None,
        }
    }
}
