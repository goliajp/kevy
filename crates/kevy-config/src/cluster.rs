//! `[cluster]` section schema — single-node cluster mode plus the
//! quorum-election peer list.
//!
//! The peer list is a flat comma-separated string
//! (`peers = "id@host:port,..."`) rather than TOML's
//! `[[array_of_tables]]`: the hand-rolled 0-dep parser does not
//! support arrays of tables, and the structural future-need is
//! bounded (per-peer TLS, auth, and region are explicitly out of
//! charter for the election subsystem).

pub use crate::peer::{PeerEntry, ScopeEntry};

/// `[cluster]` section — single-node cluster mode: keys route by
/// Redis-cluster slot (CRC16 `{hashtag}` & 16383) and every shard `i`
/// gets a second, deterministic listener at `port_base + i` that answers
/// wrong-shard keys with `-MOVED`, so stock cluster-aware clients
/// (`redis-benchmark --cluster`, `redis-cli -c`) can address shards
/// directly. The main SO_REUSEPORT port keeps full forward-anywhere
/// behaviour for non-cluster clients. Not hot-settable: the routing
/// scheme is a startup property of the data dir (`shards.meta`).
///
/// The struct is `Clone` but not `Copy` (since `peers` and `scopes`
/// hold owned vectors). Most call sites just clone the per-tick
/// `Config` snapshot via `Arc<Config>`, so this is invisible in the
/// hot path.
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str(
///     "[cluster]\nenabled = true\nport_base = 7001\n",
///     None,
/// )?;
/// assert!(cfg.cluster.enabled);
/// assert_eq!(cfg.cluster.port_base, 7001); // shard i at 7001 + i
/// assert!(cfg.cluster.peers.is_empty(), "no election configured");
/// # Ok::<(), kevy_config::ConfigError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub struct ClusterSection {
    /// Enable cluster mode. Default `false` (zero change).
    ///
    /// ```
    /// assert!(!kevy_config::Config::default().cluster.enabled);
    /// let cfg = kevy_config::Config::from_toml_str("[cluster]\nenabled = true\n", None)?;
    /// assert!(cfg.cluster.enabled);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub enabled: bool,
    /// First cluster port (shard `i` listens at `port_base + i`).
    /// `0` (default) = `server.port + 1`.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().cluster.port_base, 0); // server.port + 1
    /// let cfg = kevy_config::Config::from_toml_str("[cluster]\nport_base = 7001\n", None)?;
    /// assert_eq!(cfg.cluster.port_base, 7001);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub port_base: u16,
    /// This node's stable id for the quorum election (≤ 32 B
    /// ASCII; unique across the cluster). Default empty —
    /// `kevy-elect` is dormant unless both `node_id` and `peers`
    /// are set, so existing configs need no edit.
    ///
    /// ```
    /// assert!(kevy_config::Config::default().cluster.node_id.is_empty()); // election dormant
    /// let cfg = kevy_config::Config::from_toml_str("[cluster]\nnode_id = \"n1\"\n", None)?;
    /// assert_eq!(cfg.cluster.node_id, "n1");
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub node_id: String,
    /// First election-control listener port; shard `i` binds at
    /// `elect_port_base + i`. Default `0` → `server.port + 200`
    /// (locked by the `resolved_elect_port_base` unit test).
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().cluster.elect_port_base, 0); // server.port + 200
    /// let cfg = kevy_config::Config::from_toml_str("[cluster]\nelect_port_base = 6204\n", None)?;
    /// assert_eq!(cfg.cluster.elect_port_base, 6204);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub elect_port_base: u16,
    /// Address advertised in `CLUSTER SLOTS/NODES/SHARDS` and `-MOVED`
    /// instead of the bind address — for a node reached through a
    /// proxy or NAT. `None` (default) advertises the bind address, with
    /// `127.0.0.1` for a `0.0.0.0` bind.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str(
    ///     "[cluster]\nenabled = true\nannounce_ip = \"203.0.113.7\"\n",
    ///     None,
    /// )
    /// .unwrap();
    /// assert_eq!(cfg.cluster.announce_ip, Some([203, 0, 113, 7]));
    /// ```
    pub announce_ip: Option<[u8; 4]>,
    /// First advertised cluster port, paired with `announce_ip` when the
    /// proxy maps the per-shard ports to a different range. `0`
    /// (default) advertises the ports kevy listens on.
    ///
    /// ```
    /// let cfg =
    ///     kevy_config::Config::from_toml_str("[cluster]\nannounce_port_base = 7001\n", None).unwrap();
    /// assert_eq!(cfg.cluster.announce_port_base, 7001);
    /// ```
    pub announce_port_base: u16,
    /// Encrypt and authenticate the election links with Noise. Needs
    /// `[secure] private_key_file` and a `peer_keys` entry for every peer.
    ///
    /// ```
    /// assert!(!kevy_config::Config::default().cluster.secure);
    /// ```
    pub secure: bool,
    /// Each peer's public key, as `(node_id, key)`. A peer whose election
    /// link does not present its key is refused.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str(
    ///     &format!("[cluster]\npeer_keys = [\"n2={}\"]\n", "ab".repeat(32)),
    ///     None,
    /// )
    /// .unwrap();
    /// assert_eq!(cfg.cluster.peer_keys, vec![("n2".to_string(), [0xab; 32])]);
    /// ```
    pub peer_keys: Vec<(String, [u8; 32])>,
    /// Operator-declared peer list for `kevy-elect`. Empty when
    /// failover is not configured. Each entry is one cluster node
    /// (including potentially *this* node — kevy-elect filters
    /// self by matching `node_id`).
    ///
    /// ```
    /// use kevy_config::PeerEntry;
    ///
    /// let cfg = kevy_config::Config::from_toml_str(
    ///     "[cluster]\nnode_id = \"n1\"\npeers = [\"n1@10.0.0.1:6204\", \"n2@10.0.0.2:6204\"]\n",
    ///     None,
    /// )?;
    /// assert_eq!(cfg.cluster.peers.len(), 2);
    /// assert_eq!(cfg.cluster.peers[1], PeerEntry::new("n2".into(), "10.0.0.2".into(), 6204));
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub peers: Vec<PeerEntry>,
    /// Scope declarations: each entry pins a key prefix to a writer
    /// node (and optional fallback). Empty when scope-based
    /// multi-writer is off. Same flat-string TOML shape as `peers`
    /// — `scopes = "prefix=writer[|fallback],..."`.
    ///
    /// ```
    /// use kevy_config::ScopeEntry;
    ///
    /// let cfg = kevy_config::Config::from_toml_str(
    ///     "[cluster]\nscopes = [\"app:billing:=n1|n2\"]\n",
    ///     None,
    /// )?;
    /// let want = ScopeEntry::new(b"app:billing:".to_vec(), "n1".into()).with_fallback("n2".into());
    /// assert_eq!(cfg.cluster.scopes, vec![want]);
    /// # Ok::<(), kevy_config::ConfigError>(())
    /// ```
    pub scopes: Vec<ScopeEntry>,
}
