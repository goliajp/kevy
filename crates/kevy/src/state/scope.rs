//! Scope-routing state owned by [`RuntimeState`] — the bridge between
//! `kevy-scope` and the kevy server. Holds the [`kevy_scope::
//! OwnershipTable`] built from the `[cluster] scopes = "..."` config,
//! the peer-table lookup that resolves a writer node-id to its
//! advertised `host:port` for the `-MISDIRECTED` reply, and the
//! runtime [`MigrationTable`] the MOVE-SCOPE operator commands drive.
//!
//! Opt-in by config: empty `scopes` ⇒ this state is inert; the hot
//! dispatch path pays one `is_active` load per write to discover
//! "no scopes declared".
//!
//! [`RuntimeState`]: crate::RuntimeState

use kevy_config::Config;
use kevy_scope::{MigrationState, MigrationTable, OwnershipTable, Routing, Scope};

use super::{RuntimeState, ShardCtx};

pub(crate) struct ScopeState {
    /// Ownership table. `None` when no scopes are declared.
    ownership: Option<OwnershipTable>,
    /// Resolved `node_id → "host:port"` map for the writers +
    /// fallbacks declared in `scopes`. Built from the same `[cluster]
    /// peers` list `kevy-elect` uses. Empty when peers are not
    /// configured — in that mode `-MISDIRECTED` falls back to
    /// reporting the node id alone (the operator can still grep logs).
    peer_addrs: Vec<(String, String)>,
    /// The local node's `cluster.node_id`. `None` when not configured
    /// (scope routing is disabled in that mode — every write is
    /// "owned" by virtue of no peer set asserting otherwise).
    self_node_id: Option<String>,
    /// Runtime migration table. Operator `MOVE-SCOPE` transitions land
    /// here. `route_write` consults this **before** the static
    /// OwnershipTable so an in-flight migration returns `-QUIESCED`,
    /// and a committed migration returns `-MISDIRECTED writer is
    /// <new-writer>` regardless of the static config (until the next
    /// operator config push restarts the node with the new scope
    /// writer baked in).
    migrations: MigrationTable,
}

impl ScopeState {
    /// Build the scope state from config. Returns `Err(msg)` when the
    /// scope list fails the linter (duplicate / overlapping prefixes) —
    /// bad config should fail loudly at boot rather than at the first
    /// wrong-shard write.
    pub(crate) fn from_config(cfg: &Config) -> Result<Self, String> {
        let ownership = build_ownership(cfg)?;
        // Prefer the client-facing `client_port` if set, fall back to
        // the legacy `port` (which is the elect-control port — not what
        // a client can connect to, but kept for compat).
        let peer_addrs: Vec<(String, String)> = cfg
            .cluster
            .peers
            .iter()
            .map(|p| {
                let reported = p.client_port.unwrap_or(p.port);
                (p.node_id.clone(), format!("{}:{}", p.host, reported))
            })
            .collect();
        let self_node_id = if cfg.cluster.node_id.is_empty() {
            None
        } else {
            Some(cfg.cluster.node_id.clone())
        };
        Ok(Self {
            ownership,
            peer_addrs,
            self_node_id,
            migrations: MigrationTable::new(),
        })
    }


    /// `true` when the ownership table has at least one declared
    /// scope. The hot dispatch path branches off this early — when
    /// the table is empty (the common single-writer case),
    /// `route_write` is never called.
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.ownership.is_some()
    }

    /// Self node id accessor. The MOVE-SCOPE command handler reads
    /// this to validate `<from-id>` matches the local node before
    /// starting a migration.
    pub(crate) fn self_node_id(&self) -> Option<&str> {
        self.self_node_id.as_deref()
    }

    /// Resolve a node id to its advertised `host:port` (used by the
    /// MOVE-SCOPE command handler — it needs to connect to the target
    /// writer over TCP).
    pub(crate) fn peer_addr(&self, node_id: &str) -> Option<String> {
        self.peer_addrs
            .iter()
            .find(|(id, _)| id == node_id)
            .map(|(_, addr)| addr.clone())
    }

    pub(crate) fn migration_start(
        &self,
        prefix: Vec<u8>,
        from: String,
        to: String,
    ) -> Result<(), kevy_scope::MigrationError> {
        self.migrations.start(prefix, from, to)
    }

    pub(crate) fn migration_commit(&self, prefix: &[u8]) -> Option<MigrationState> {
        self.migrations.commit(prefix)
    }

    pub(crate) fn migration_abort(&self, prefix: &[u8]) -> Option<MigrationState> {
        self.migrations.abort(prefix)
    }

    fn resolve_addr(&self, node_id: &str) -> String {
        self.peer_addr(node_id)
            .unwrap_or_else(|| node_id.to_string())
    }
}

/// Verdict from [`RuntimeState::route_write`]. The cement layer
/// pattern-matches to pick the right wire encoding.
pub(crate) enum WriteRedirect {
    /// `-MISDIRECTED writer is <addr>`.
    Misdirected(String),
    /// `-QUIESCED migrating to <addr>`. Quiesced is transient —
    /// once the migration commits or aborts, future writes
    /// transition to Misdirected (or back to Owned).
    Quiesced { to_addr: String },
}

impl RuntimeState {
    /// Route a write for `key`. Decision order:
    /// 1. INGESTING — the shard is inside a MOVE-SCOPE-INGEST handler
    ///    for a matching prefix; every embedded write must apply
    ///    locally even though the static config says we're not the
    ///    writer, so this overrides every other routing rule.
    /// 2. MIGRATING — `-QUIESCED <to-addr>` (caller handles encoding).
    /// 3. MIGRATED — `-MISDIRECTED <to-addr>` (committed move
    ///    overrides static config).
    /// 4. Static OwnershipTable + fallback via elect snapshot: when
    ///    the snapshot reports the scope's writer in `down_peers`, the
    ///    declared fallback is treated as the active owner.
    ///
    /// Return value:
    /// - `None` — local node accepts the write (owned, or no scope
    ///   applies).
    /// - `Some(WriteRedirect::…)` — encode the matching redirect.
    pub(crate) fn route_write(&self, key: &[u8], shard: &ShardCtx) -> Option<WriteRedirect> {
        if shard.ingesting_matches(key) {
            return None;
        }
        let scope = &self.scope;
        if let Some(m) = scope.migrations.match_migrating(key) {
            return Some(WriteRedirect::Quiesced {
                to_addr: scope.resolve_addr(&m.to),
            });
        }
        if let Some(m) = scope.migrations.match_migrated(key) {
            return Some(WriteRedirect::Misdirected(scope.resolve_addr(&m.to)));
        }
        let table = scope.ownership.as_ref()?;
        let self_id = scope.self_node_id()?;
        let routing = match self.election.current_snapshot() {
            Some(snap) => table.route_with_fallback_state(key, self_id, |id| {
                snap.down_peers.iter().any(|d| d == id)
            }),
            None => table.route(key, self_id),
        };
        match routing {
            Routing::Owned | Routing::Unknown => None,
            Routing::Misdirected { target } => {
                Some(WriteRedirect::Misdirected(scope.resolve_addr(target)))
            }
        }
    }
}

/// Encode a `-MISDIRECTED writer is <host:port>` RESP error onto
/// `out`. Format mirrors the Redis Cluster `-MOVED` convention so
/// existing client libraries can pattern-match the prefix.
pub(crate) fn encode_misdirected(out: &mut Vec<u8>, target: &str) {
    out.extend_from_slice(b"-MISDIRECTED writer is ");
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// `-QUIESCED migrating to <host:port>` — the transient reply
/// during the MOVE-SCOPE quiesce window.
pub(crate) fn encode_quiesced(out: &mut Vec<u8>, target: &str) {
    out.extend_from_slice(b"-QUIESCED migrating to ");
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// Build the scope [`OwnershipTable`] from `[cluster] scopes`,
/// warning loudly at boot about scopes with no fallback declared
/// (a legal availability trade-off, but one the operator should
/// see).
fn build_ownership(cfg: &Config) -> Result<Option<OwnershipTable>, String> {
    if cfg.cluster.scopes.is_empty() {
        return Ok(None);
    }
    let scopes: Vec<Scope> = cfg
        .cluster
        .scopes
        .iter()
        .map(|e| {
            let s = Scope::new(e.prefix.clone(), e.writer.clone());
            match e.fallback.as_ref() {
                Some(fb) => s.with_fallback(fb.clone()),
                None => s,
            }
        })
        .collect();
    let table = OwnershipTable::new(scopes).map_err(|e| e.to_string())?;
    for s in table.scopes_without_fallback() {
        let prefix_lossy = String::from_utf8_lossy(s.prefix());
        eprintln!(
            "kevy: WARN scope {prefix_lossy:?} has no fallback declared — writes for this scope fail if writer {:?} is DOWN",
            s.writer(),
        );
    }
    Ok(Some(table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_misdirected_wire_shape() {
        let mut out = Vec::new();
        encode_misdirected(&mut out, "10.0.0.1:6004");
        assert_eq!(out, b"-MISDIRECTED writer is 10.0.0.1:6004\r\n");
    }

    #[test]
    fn resolve_addr_falls_back_to_node_id_when_unmapped() {
        let scope = ScopeState::from_config(&Config::default()).unwrap();
        assert_eq!(scope.resolve_addr("kevy-some-unmapped-id"), "kevy-some-unmapped-id");
    }

    #[test]
    fn from_config_default_is_inert() {
        let scope = ScopeState::from_config(&Config::default()).unwrap();
        assert!(!scope.is_active());
        assert!(scope.self_node_id().is_none());
    }
}

