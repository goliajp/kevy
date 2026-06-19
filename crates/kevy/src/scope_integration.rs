//! Bridge between `kevy-scope` and the kevy server. Holds a single
//! process-global [`kevy_scope::OwnershipTable`] built from the
//! `[cluster] scopes = "..."` config at startup, plus the peer-table
//! lookup that resolves a writer node-id to its advertised
//! `host:port` for the `-MISDIRECTED` reply (T3.8).
//!
//! Opt-in by config: empty `scopes` ⇒ this module is a no-op
//! beyond the initial parse-check. The hot dispatch path pays one
//! relaxed atomic load per write to discover "no scopes declared".

use std::sync::OnceLock;

use kevy_config::Config;
use kevy_scope::{MigrationState, MigrationTable, OwnershipTable, Routing, Scope};

/// Process-global ownership table. `None` when no scopes are
/// declared; `Some(Arc<OwnershipTable>)` after [`install`]
/// (called once during `kevy::serve` startup).
static OWNERSHIP: OnceLock<Option<OwnershipTable>> = OnceLock::new();

/// Process-global resolved `node_id → "host:port"` map for the
/// writers + fallbacks declared in `scopes`. Built from the same
/// `[cluster] peers` list `kevy-elect` uses. Empty when peers are
/// not configured — in that mode `-MISDIRECTED` falls back to
/// reporting the node id alone (the operator can still grep logs).
static PEER_ADDRS: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// Process-global migration table (T3.14). Operator `MOVE-SCOPE`
/// transitions land here. `route_write` consults this **before**
/// the static OwnershipTable so an in-flight migration returns
/// `-QUIESCED`, and a committed migration returns `-MISDIRECTED
/// writer is <new-writer>` regardless of the static config (until
/// the next operator config push restarts the node with the new
/// scope writer baked in).
static MIGRATIONS: OnceLock<MigrationTable> = OnceLock::new();

fn migrations() -> &'static MigrationTable {
    MIGRATIONS.get_or_init(MigrationTable::new)
}

/// Public-ish surface for the MOVE-SCOPE command handlers (server
/// cement in `crates/kevy/src/ops/`). Pre-T3.14.5 the actual data
/// ship runs out-of-band; these three transitions are the state-
/// machine portion.
#[allow(dead_code)] // wired by the MOVE-SCOPE command in T3.14.5
pub(crate) fn migration_start(
    prefix: Vec<u8>,
    from: String,
    to: String,
) -> Result<(), kevy_scope::MigrationError> {
    migrations().start(prefix, from, to)
}

#[allow(dead_code)] // wired by the MOVE-SCOPE command in T3.14.5
pub(crate) fn migration_commit(prefix: &[u8]) -> Option<MigrationState> {
    migrations().commit(prefix)
}

pub(crate) fn migration_abort(prefix: &[u8]) -> Option<MigrationState> {
    migrations().abort(prefix)
}

thread_local! {
    /// Set on the reactor thread for the duration of a
    /// `MOVE-SCOPE-INGEST` dispatch. `route_write` checks this
    /// FIRST and returns `None` (= locally writable) when the key
    /// matches the ingesting prefix — the target node accepts
    /// the source's reconstruction commands without bouncing
    /// them back via `-MISDIRECTED`.
    static INGESTING_PREFIX: std::cell::RefCell<Option<Vec<u8>>> =
        const { std::cell::RefCell::new(None) };
}

/// RAII guard set by the MOVE-SCOPE-INGEST handler. Cleared on
/// drop; nested guards aren't expected (a recursion into another
/// MOVE-SCOPE-INGEST inside one would be a bug — the inner ingest
/// silently inherits the outer's prefix until both drop).
pub(crate) struct IngestGuard {
    _priv: (),
}

impl IngestGuard {
    pub(crate) fn enter(prefix: Vec<u8>) -> Self {
        INGESTING_PREFIX.with(|c| *c.borrow_mut() = Some(prefix));
        Self { _priv: () }
    }
}

impl Drop for IngestGuard {
    fn drop(&mut self) {
        INGESTING_PREFIX.with(|c| *c.borrow_mut() = None);
    }
}

fn is_ingesting_match(key: &[u8]) -> bool {
    INGESTING_PREFIX.with(|c| c.borrow().as_ref().is_some_and(|p| key.starts_with(p)))
}

/// Self node id accessor (read-only). The MOVE-SCOPE command
/// handler reads this to validate `<from-id>` matches the local
/// node before starting a migration.
pub(crate) fn self_node_id() -> Option<&'static str> {
    SELF_NODE_ID.get().and_then(Option::as_deref)
}

/// Install the ownership table + peer-address map. Idempotent —
/// the OnceLocks accept only the first set; subsequent calls
/// silently no-op. Called from `kevy::serve` before
/// `runtime.run`.
///
/// Returns `Err(msg)` when the scope list fails the linter
/// (duplicate / overlapping prefixes). The server caller
/// `eprintln!`s + exits — bad config should fail loudly at boot
/// rather than at the first wrong-shard write.
pub(crate) fn install(cfg: &Config) -> Result<(), String> {
    // Build the OwnershipTable iff any scopes are declared.
    if !cfg.cluster.scopes.is_empty() {
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
        // T3.13: warn about scopes without a fallback declared.
        // Not an error (the operator may want availability =
        // writer-only), but the trade-off should be loud at boot.
        for s in table.scopes_without_fallback() {
            let prefix_lossy = String::from_utf8_lossy(s.prefix());
            eprintln!(
                "kevy: WARN scope {prefix_lossy:?} has no fallback declared — writes for this scope fail if writer {:?} is DOWN",
                s.writer(),
            );
        }
        let _ = OWNERSHIP.set(Some(table));
    } else {
        let _ = OWNERSHIP.set(None);
    }
    // Build the peer-address map from `[cluster] peers`. Empty
    // peer list ⇒ empty map (the MISDIRECTED encoder will use the
    // node id alone).
    let peers: Vec<(String, String)> = cfg
        .cluster
        .peers
        .iter()
        .map(|p| (p.node_id.clone(), format!("{}:{}", p.host, p.port)))
        .collect();
    let _ = PEER_ADDRS.set(peers);
    Ok(())
}

/// `true` when the ownership table has at least one declared
/// scope. The hot dispatch path branches off this early — when
/// the table is empty (the common single-writer case),
/// `route_write` is never called.
#[inline]
pub(crate) fn is_active() -> bool {
    matches!(OWNERSHIP.get(), Some(Some(_)))
}

/// The local node's `cluster.node_id`. Resolved lazily on first
/// call and cached. `None` when not configured (scope routing is
/// disabled in that mode — every write is "owned" by virtue of
/// no peer set asserting otherwise).
static SELF_NODE_ID: OnceLock<Option<String>> = OnceLock::new();

pub(crate) fn install_self_id(cfg: &Config) {
    let id = if cfg.cluster.node_id.is_empty() {
        None
    } else {
        Some(cfg.cluster.node_id.clone())
    };
    let _ = SELF_NODE_ID.set(id);
}

/// Route a write for `key`: `None` when the local node is the
/// writer (or the active fallback when the writer is DOWN per
/// `kevy-elect`), `Some(target_host_port)` when the cement should
/// encode `-MISDIRECTED writer is <target>`. T3.11 fallback path:
/// when the elect snapshot reports the scope's writer in
/// `down_peers`, the declared fallback is treated as the active
/// owner — exactly the F4 contract from the RFC.
/// Route a write for `key`. Decision order (T3.14):
/// 1. MIGRATING — `-QUIESCED <to-addr>` (caller handles encoding).
/// 2. MIGRATED — `-MISDIRECTED <to-addr>` (committed move overrides
///    static config).
/// 3. Static OwnershipTable + F4 fallback via elect snapshot.
///
/// Return value:
/// - `None` — local node accepts the write (owned, or no scope
///   applies).
/// - `Some(WriteRedirect::Misdirected(addr))` — encode
///   `-MISDIRECTED writer is <addr>`.
/// - `Some(WriteRedirect::Quiesced { prefix_lossy, to_addr })` —
///   encode `-QUIESCED <prefix> migrating to <addr>`.
pub(crate) fn route_write(key: &[u8]) -> Option<WriteRedirect> {
    // T3.14.5 ingest bypass: while the local reactor thread is
    // running a MOVE-SCOPE-INGEST handler for a matching prefix,
    // every embedded write must apply locally even though the
    // static config says we're not the writer. The guard fires
    // FIRST so it overrides every other routing rule.
    if is_ingesting_match(key) {
        return None;
    }
    // Step 1 & 2 — runtime migration state (read FIRST so it can
    // override the static config).
    if let Some(m) = migrations().match_migrating(key) {
        return Some(WriteRedirect::Quiesced {
            to_addr: resolve_addr(&m.to),
        });
    }
    if let Some(m) = migrations().match_migrated(key) {
        return Some(WriteRedirect::Misdirected(resolve_addr(&m.to)));
    }
    // Step 3 — static OwnershipTable + F4 fallback.
    let table = OWNERSHIP.get()?.as_ref()?;
    let self_id = self_node_id()?;
    let routing = match crate::elect_integration::current_snapshot() {
        Some(snap) => table.route_with_fallback_state(key, self_id, |id| {
            snap.down_peers.iter().any(|d| d == id)
        }),
        None => table.route(key, self_id),
    };
    match routing {
        Routing::Owned | Routing::Unknown => None,
        Routing::Misdirected { target } => Some(WriteRedirect::Misdirected(resolve_addr(target))),
    }
}

/// Verdict from [`route_write`]. The cement layer pattern-matches
/// to pick the right wire encoding.
pub(crate) enum WriteRedirect {
    /// `-MISDIRECTED writer is <addr>`.
    Misdirected(String),
    /// `-QUIESCED migrating to <addr>`. Quiesced is transient —
    /// once the migration commits or aborts, future writes
    /// transition to Misdirected (or back to Owned).
    Quiesced { to_addr: String },
}

fn resolve_addr(node_id: &str) -> String {
    let Some(peers) = PEER_ADDRS.get() else {
        return node_id.to_string();
    };
    peers
        .iter()
        .find(|(id, _)| id == node_id)
        .map(|(_, addr)| addr.clone())
        .unwrap_or_else(|| node_id.to_string())
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
/// during the MOVE-SCOPE quiesce window (T3.14 / Q3 = a).
pub(crate) fn encode_quiesced(out: &mut Vec<u8>, target: &str) {
    out.extend_from_slice(b"-QUIESCED migrating to ");
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// Resolve a node id to its advertised `host:port` (public API for
/// the MOVE-SCOPE command handler — it needs to connect to the
/// target writer over TCP).
#[allow(dead_code)] // wired by the MOVE-SCOPE command in T3.14.5
pub(crate) fn peer_addr(node_id: &str) -> Option<String> {
    let peers = PEER_ADDRS.get()?;
    peers
        .iter()
        .find(|(id, _)| id == node_id)
        .map(|(_, addr)| addr.clone())
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
    fn resolve_addr_falls_back_to_node_id_when_no_peers() {
        // Without setting PEER_ADDRS, the resolver returns the
        // node id verbatim. The OnceLock state is process-global,
        // so this test runs only when PEER_ADDRS hasn't been set
        // by a prior test in the same process. Use a randomized
        // id so the assertion is meaningful either way.
        let v = resolve_addr("kevy-some-unmapped-id");
        // Either we get the raw id back (no peers set), or we get
        // the raw id back (peers set but no match).
        assert_eq!(v, "kevy-some-unmapped-id");
    }
}
