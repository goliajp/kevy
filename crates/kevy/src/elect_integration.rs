//! Bridge between `kevy-elect` and the kevy server. When the
//! operator configures `[cluster] peers = "..."` + `node_id`, this
//! module brings up a single per-process `Transport` (election is
//! per-node, not per-shard) and holds its handle for the lifetime
//! of `kevy::serve`. ROLE / INFO replication observe the Transport
//! via a process-global slot; the Transport's `set_repl_offset`
//! receives the answering shard's offset from the per-tick
//! `Commands::on_replication_view` hook (shard 0's snapshot — see
//! note in `set_view_offset` below).
//!
//! Opt-in by config: empty `peers` ⇒ this module is a no-op
//! beyond the initial parse-check.

use std::cell::Cell;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use kevy_config::{Config, PeerEntry, ReplicationRole};
use kevy_elect::{
    PeerAddr, Transport,
    elector::{ElectConfig, ElectJitter, Elector},
    message::Role,
};

/// Process-global Transport handle. `None` when the operator has
/// not configured peers (v1.18-era deployment), `Some(...)` while
/// the elector is live. Read by `ROLE` / `INFO replication` for the
/// live role view.
static ELECT_TRANSPORT: OnceLock<Mutex<Option<Transport>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Transport>> {
    ELECT_TRANSPORT.get_or_init(|| Mutex::new(None))
}

/// Per-shard `master_repl_offset` slot for the elect aggregator
/// (initialised lazily to a fixed array on first `set_view_offset`
/// from each shard; the cluster size is fixed at boot so a Vec is
/// always exactly `nshards` long).
///
/// Reader: `aggregate_offset` sums every slot (saturating). Sum is
/// chosen over `max` because it captures **total applied work**
/// across shards — a node with more writes applied in aggregate is
/// "more current" for "highest offset wins" candidate selection.
/// A pure `max` would mean a node with one busy shard ties with a
/// node where every shard advanced equally.
static SHARD_OFFSETS: OnceLock<Vec<AtomicU64>> = OnceLock::new();

thread_local! {
    /// Thread-per-core: the reactor thread *is* the shard. The
    /// first `set_view_offset` from this thread learns its own
    /// shard id by hooking into the existing `ops::cluster::
    /// current_shard()` value the kevy server already publishes.
    /// `None` ⇒ not yet learned (very first tick).
    static MY_SHARD_ID: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Allocate the per-shard offset slot vector. Called once at
/// startup from `kevy::serve` before any shard starts running.
pub(crate) fn install_shard_offsets(nshards: usize) {
    let _ = SHARD_OFFSETS.set((0..nshards).map(|_| AtomicU64::new(0)).collect());
}

fn shard_offsets() -> Option<&'static [AtomicU64]> {
    SHARD_OFFSETS.get().map(Vec::as_slice)
}

fn aggregate_offset() -> u64 {
    shard_offsets()
        .map(|slots| {
            slots
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .fold(0u64, u64::saturating_add)
        })
        .unwrap_or(0)
}

/// Decide whether kevy-elect is wired up at all. False when the
/// operator left `peers` empty (v1.18-era deployment) or `node_id`
/// blank; the rest of the integration short-circuits accordingly.
pub(crate) fn is_configured(cfg: &Config) -> bool {
    !cfg.cluster.peers.is_empty() && !cfg.cluster.node_id.is_empty()
}

/// Start the `Transport` if `peers` + `node_id` are configured. No-
/// op otherwise. Called once from `kevy::serve` before `runtime.run`.
///
/// Logs (`kevy:` prefix) and returns without crashing on any
/// startup error — kevy-elect's failure mode is "no automatic
/// failover available"; the data plane keeps working with the
/// v1.18 manual `REPLICAOF` semantics.
pub(crate) fn maybe_start(cfg: &Config) {
    if !is_configured(cfg) {
        return;
    }
    let listen_port = resolved_elect_port_base(cfg);
    let start_role = match cfg.replication.role {
        ReplicationRole::Primary => Role::Primary,
        _ => Role::Replica,
    };
    let peer_ids: Vec<String> = cfg
        .cluster
        .peers
        .iter()
        .map(|p| p.node_id.clone())
        .collect();
    let advertised_addr = format!("{}:{}", advertised_host(cfg), cfg.server.port);
    let elect_cfg = ElectConfig::default();
    let hb_interval = elect_cfg.hb_interval;
    let elector = Elector::new(
        cfg.cluster.node_id.clone(),
        peer_ids,
        advertised_addr,
        start_role,
        elect_cfg,
        ElectJitter::System,
    );
    // Filter out self when building outbound `PeerAddr` list.
    let self_id = cfg.cluster.node_id.as_str();
    let peers: Vec<PeerAddr> = cfg
        .cluster
        .peers
        .iter()
        .filter(|p| p.node_id != self_id)
        .map(peer_to_addr)
        .collect();
    let listen = (IpAddr::V4(Ipv4Addr::new(
        cfg.server.bind[0],
        cfg.server.bind[1],
        cfg.server.bind[2],
        cfg.server.bind[3],
    )), listen_port);
    match Transport::spawn(elector, hb_interval, listen, peers) {
        Ok(t) => {
            *slot().lock().expect("ELECT_TRANSPORT poisoned") = Some(t);
            eprintln!(
                "kevy: kevy-elect transport up on {}:{} ({} peers, role={})",
                cfg.server.bind[0], listen_port,
                cfg.cluster.peers.len().saturating_sub(1),
                if matches!(start_role, Role::Primary) { "primary" } else { "replica" },
            );
        }
        Err(e) => {
            eprintln!("kevy: kevy-elect transport failed to bind {listen_port}: {e}");
        }
    }
}

/// Stop the `Transport` if one is running. Called from the
/// `kevy::serve` shutdown path; idempotent.
pub(crate) fn shutdown() {
    if let Ok(mut guard) = slot().lock()
        && let Some(t) = guard.take()
    {
        t.shutdown();
    }
}

/// Read the live elector view (`role` / `epoch` / `current_primary`).
/// Returns `None` when the Transport isn't running (i.e. `peers` is
/// empty in config). `cmd_role` + `info_replication` read this to
/// override the static-config role flag with the live election
/// state.
pub(crate) fn current_snapshot() -> Option<kevy_elect::ElectorSnapshot> {
    slot().lock().ok().and_then(|g| g.as_ref().map(Transport::state_snapshot))
}

/// Feed the answering shard's `master_repl_offset` into the
/// elector. Called from `Commands::on_replication_view` per tick
/// per shard. Each shard writes its own slot; `aggregate_offset`
/// sums across shards (`sum`, not `max` — total applied work, see
/// the doc on `SHARD_OFFSETS`). The Transport's `set_repl_offset`
/// receives the aggregate, so HBs from a multi-shard node always
/// carry a stable cluster-wide signal.
pub(crate) fn set_view_offset(offset: u64) {
    // Determine which shard this thread represents — read from the
    // `ops::cluster::current_shard` thread-local the kevy server
    // already publishes on `on_shard_start`.
    let shard_id = MY_SHARD_ID.with(|c| {
        if let Some(id) = c.get() {
            return id;
        }
        let id = crate::ops::cluster::current_shard_for_elect();
        c.set(Some(id));
        id
    });
    if let Some(slots) = shard_offsets()
        && let Some(slot_ref) = slots.get(shard_id)
    {
        slot_ref.store(offset, Ordering::Relaxed);
    }
    let agg = aggregate_offset();
    if let Ok(guard) = slot().lock()
        && let Some(t) = guard.as_ref()
    {
        t.set_repl_offset(agg);
    }
}

fn resolved_elect_port_base(cfg: &Config) -> u16 {
    if cfg.cluster.elect_port_base != 0 {
        return cfg.cluster.elect_port_base;
    }
    // Default: `server.port + 200` so it doesn't collide with the
    // replication listener (server.port + 10000 by default in v1.18)
    // or the cluster port (server.port + 1 in cluster mode).
    cfg.server.port.saturating_add(200)
}

fn advertised_host(cfg: &Config) -> String {
    // Use the bind address as the advertised host. Operators behind
    // NAT will want to set an external IP via a future config knob —
    // v1.19 ships with the bind address only; if the bind is
    // 0.0.0.0 (all interfaces), the advertised string is still
    // 0.0.0.0 (caller-resolved by the peer's hostname mapping).
    format!(
        "{}.{}.{}.{}",
        cfg.server.bind[0], cfg.server.bind[1], cfg.server.bind[2], cfg.server.bind[3]
    )
}

fn peer_to_addr(p: &PeerEntry) -> PeerAddr {
    PeerAddr {
        node_id: p.node_id.clone(),
        host: p.host.clone(),
        port: p.port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(node_id: &str, peers: &str) -> Config {
        let mut c = Config::default();
        c.cluster.node_id = node_id.to_string();
        c.cluster.peers = PeerEntry::parse_list(peers).unwrap();
        c
    }

    #[test]
    fn is_configured_empty_peers_returns_false() {
        let cfg = Config::default();
        assert!(!is_configured(&cfg));
    }

    #[test]
    fn is_configured_empty_node_id_returns_false() {
        let mut cfg = Config::default();
        cfg.cluster.peers = PeerEntry::parse_list("a@h:1,b@h:2").unwrap();
        // node_id is empty.
        assert!(!is_configured(&cfg));
    }

    #[test]
    fn is_configured_both_set_returns_true() {
        let cfg = cfg_with("self", "self@127.0.0.1:1,b@127.0.0.1:2");
        assert!(is_configured(&cfg));
    }

    #[test]
    fn resolved_elect_port_base_falls_back_when_zero() {
        let mut cfg = Config::default();
        cfg.server.port = 6004;
        cfg.cluster.elect_port_base = 0;
        assert_eq!(resolved_elect_port_base(&cfg), 6204);
    }

    #[test]
    fn resolved_elect_port_base_uses_explicit_when_set() {
        let mut cfg = Config::default();
        cfg.cluster.elect_port_base = 16104;
        assert_eq!(resolved_elect_port_base(&cfg), 16104);
    }
}
