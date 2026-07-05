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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationSection {
    /// Node role. `Standalone` (default) disables the whole subsystem.
    pub role: ReplicationRole,
    /// `host:port` of the primary, for `role = "replica"`. Ignored when
    /// `role != "replica"`. `None` for replica = config-error at startup.
    pub upstream: Option<String>,
    /// TCP port BASE the primary listens on for incoming replica
    /// connections — shard `i` binds at `listen_port_base + i`, mirroring
    /// the cluster listener pattern. `0` (default) = `server.port +
    /// 10000` as the base, picked at startup. Only meaningful when
    /// `role = "primary"`. Replicas use a shard-aware client that
    /// connects to all `nshards` ports to mirror the full keyspace.
    pub listen_port_base: u16,
    /// Bounded ring buffer for recent applied frames (bytes). A replica
    /// that disconnects and reconnects within this backlog window catches
    /// up without a full snapshot. Default `256mb`.
    pub replication_buffer_size: u64,
    /// How long the primary keeps a disconnected replica's slot before
    /// dropping it (and forcing a full snapshot on reconnect). Default
    /// `60_000` (60 s).
    pub reconnect_window_ms: u32,
    /// v3.14 D5 — primary refuses writes when fewer than this many
    /// replicas have a live ACK within `min_replicas_max_lag_ms`.
    /// `0` (default) disables the check. A lag heuristic, not a
    /// quorum guarantee — the quorum lease (v3.16) is the real
    /// split-brain fence.
    pub min_replicas_to_write: u32,
    /// Freshness window for `min_replicas_to_write` (ms).
    pub min_replicas_max_lag_ms: u32,
    /// v3.14 — reject client writes while in the replica role
    /// (`-READONLY`). Default `true`, Redis-compatible; the
    /// replication apply path and admin verbs bypass the gate.
    pub replica_read_only: bool,
    /// v3.2 — the upstream is a SINGLE stream on one port (an embedded
    /// writer's `embed_writer_listen_addr` source) rather than a
    /// per-shard port fleet: one routing runner fans frames into local
    /// shards by key hash. Only meaningful when `role = "replica"`.
    pub single_source: bool,
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
            replica_read_only: true,
            single_source: false,
        }
    }
}

/// Node role for the `[replication]` subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplicationRole {
    /// Default. Replication subsystem dormant; behaves like pre-v3.
    #[default]
    Standalone,
    /// This node accepts writes and streams mutations to replicas.
    Primary,
    /// This node connects to a primary and mirrors its keyspace read-only.
    Replica,
}

impl ReplicationRole {
    /// Canonical name used by `CONFIG GET replication.role` and TOML.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Primary => "primary",
            Self::Replica => "replica",
        }
    }
    /// Inverse of [`Self::as_str`] — case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "standalone" => Some(Self::Standalone),
            "primary" => Some(Self::Primary),
            "replica" => Some(Self::Replica),
            _ => None,
        }
    }
}
