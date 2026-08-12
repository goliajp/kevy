//! Election-plane state owned by [`RuntimeState`] — the bridge between
//! `kevy-elect` and the kevy server. When the operator configures
//! `[cluster] peers = "..."` + `node_id`, [`ElectionState::maybe_start`]
//! brings up a single per-process `Transport` (election is per-node,
//! not per-shard) and holds its handle for the lifetime of
//! `kevy::serve`. ROLE / INFO replication observe the Transport via
//! [`ElectionState::current_snapshot`]; the Transport's
//! `set_repl_offset` receives each shard's offset from the per-tick
//! `Commands::on_replication_view` hook.
//!
//! Opt-in by config: empty `peers` ⇒ this state stays dormant (the
//! transport slot is `None`, the offset slots idle).
//!
//! [`RuntimeState`]: crate::RuntimeState

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use kevy_config::{Config, PeerEntry, ReplicationRole};
use kevy_elect::{
    PeerAddr, Transport,
    elector::{ElectConfig, ElectJitter, Elector},
    message::Role,
};

use super::ReplicationState;

pub(crate) struct ElectionState {
    /// Live Transport handle. `None` when the operator has not
    /// configured peers, `Some(...)` while the elector is running.
    transport: RwLock<Option<Transport>>,
    /// Per-shard `master_repl_offset` slot for the elect aggregator.
    ///
    /// Reader: [`Self::aggregate_offset`] sums every slot (saturating).
    /// Sum is chosen over `max` because it captures **total applied
    /// work** across shards — a node with more writes applied in
    /// aggregate is "more current" for "highest offset wins" candidate
    /// selection. A pure `max` would mean a node with one busy shard
    /// ties with a node where every shard advanced equally.
    shard_offsets: Box<[AtomicU64]>,
}

impl ElectionState {
    pub(crate) fn new(nshards: usize) -> Self {
        Self {
            transport: RwLock::new(None),
            shard_offsets: (0..nshards.max(1)).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// Start the `Transport` if `peers` + `node_id` are configured. No-
    /// op otherwise. Called once from `kevy::serve` before `runtime.run`.
    ///
    /// Logs (`kevy:` prefix) and returns without crashing on any
    /// startup error — kevy-elect's failure mode is "no automatic
    /// failover available"; the data plane keeps working with the
    /// manual `REPLICAOF` semantics.
    pub(crate) fn maybe_start(&self, cfg: &Config, replication: &Arc<ReplicationState>) {
        if !is_configured(cfg) {
            return;
        }
        let listen_port = resolved_elect_port_base(cfg);
        let elect_cfg = ElectConfig::default();
        let hb_interval = elect_cfg.hb_interval;
        let (elector, start_role) = build_elector(cfg, elect_cfg, replication);
        // Filter out self when building outbound `PeerAddr` list.
        let self_id = cfg.cluster.node_id.as_str();
        let peers: Vec<PeerAddr> = cfg
            .cluster
            .peers
            .iter()
            .filter(|p| p.node_id != self_id)
            .map(peer_to_addr)
            .collect();
        let listen = (
            IpAddr::V4(Ipv4Addr::new(
                cfg.server.bind[0],
                cfg.server.bind[1],
                cfg.server.bind[2],
                cfg.server.bind[3],
            )),
            listen_port,
        );
        let on_change = make_topology_callback(cfg, Arc::clone(replication));
        match Transport::spawn_with_callback(elector, hb_interval, listen, peers, on_change) {
            Ok(t) => {
                *self.transport.write().expect("elect transport poisoned") = Some(t);
                eprintln!(
                    "kevy: kevy-elect transport up on {}:{} ({} peers, role={})",
                    cfg.server.bind[0],
                    listen_port,
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
    pub(crate) fn shutdown(&self) {
        if let Ok(mut guard) = self.transport.write()
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
    pub(crate) fn current_snapshot(&self) -> Option<kevy_elect::ElectorSnapshot> {
        self.transport
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(Transport::state_snapshot))
    }

    /// Feed shard `shard_id`'s `master_repl_offset` into the elector.
    /// Called from `Commands::on_replication_view` per tick per shard.
    /// Each shard writes its own slot; [`Self::aggregate_offset`] sums
    /// across shards, so HBs from a multi-shard node always carry a
    /// stable cluster-wide signal.
    ///
    /// Role-aware source: while this node runs as a REPLICA the
    /// shard-local `master_repl_offset` does not measure how much of
    /// the upstream stream it has applied, so the aggregate switches to
    /// `replication.applied_offset_sum()` (per-runner applied stream
    /// positions, summed). Both roles thus report the same unit —
    /// "replication-stream position, totalled across streams" — which is
    /// what `am_best_candidate`'s highest-offset-wins ordering compares.
    pub(crate) fn set_view_offset(
        &self,
        replication: &ReplicationState,
        shard_id: usize,
        offset: u64,
    ) {
        if let Some(slot) = self.shard_offsets.get(shard_id) {
            slot.store(offset, Ordering::Relaxed);
        }
        let agg = if replication.is_replica() {
            replication.applied_offset_sum()
        } else {
            self.aggregate_offset()
        };
        if let Ok(guard) = self.transport.read()
            && let Some(t) = guard.as_ref()
        {
            t.set_repl_offset(agg);
        }
    }

    fn aggregate_offset(&self) -> u64 {
        self.shard_offsets
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .fold(0u64, u64::saturating_add)
    }
}

/// Decide whether kevy-elect is wired up at all. False when the
/// operator left `peers` empty or `node_id` blank; the rest of the
/// integration short-circuits accordingly.
fn is_configured(cfg: &Config) -> bool {
    !cfg.cluster.peers.is_empty() && !cfg.cluster.node_id.is_empty()
}

/// Failover closing-of-the-loop: election outcomes drive the DATA
/// plane. The returned callback runs on the elect orchestrator thread;
/// both actions are quick (runner spawn/stop, no blocking I/O in the
/// caller's path). Membership is the STATIC config table; only roles
/// are dynamic. The callback captures the narrow
/// `Arc<ReplicationState>` slice — never the whole `RuntimeState`.
fn make_topology_callback(
    cfg: &Config,
    replication: Arc<ReplicationState>,
) -> kevy_elect::TopologyCallback {
    let member_table: Vec<(String, String, u16)> = cfg
        .cluster
        .peers
        .iter()
        .map(|p| (p.node_id.clone(), p.host.clone(), p.client_port.unwrap_or(p.port)))
        .collect();
    let my_id = cfg.cluster.node_id.clone();
    Box::new(move |role, primary, quorum| {
        use kevy_elect::Role;
        // Primary quorum lease: a primary that cannot see a strict
        // majority fences writes (the partition's minority side must
        // not keep absorbing them). Replicas never fence here — they
        // are already read-only.
        let fence = matches!(role, Role::Primary) && !quorum;
        if replication.set_quorum_fence(fence) {
            eprintln!(
                "kevy: elect — quorum lease {}",
                if fence { "LOST: writes fenced" } else { "restored: writes open" }
            );
        }
        match (role, primary) {
            (Role::Primary, _) => {
                // We won (or started as primary): stop any replica
                // runners — REPLICAOF NO ONE semantics. Promotion
                // (we WERE a replica) additionally bumps the
                // promotion counter → per-shard feed-gen fence.
                replication.promote_stop_runners();
                eprintln!("kevy: elect — this node is PRIMARY (writes open)");
                if kevy_rt::repl_trace() {
                    kevy_rt::repl_trace_line(format_args!(
                        "elect: promotion epoch bumped — writes open NOW, \
                         per-shard feed bumps trail on each shard's tick"
                    ));
                }
            }
            (Role::Replica, Some(pid)) if pid != my_id => {
                follow_new_primary(&replication, &member_table, &pid);
            }
            _ => {}
        }
    })
}

/// Retarget this node's replica runners at a newly announced
/// primary, resolving its replication address from the static
/// member table (client port + 10000, the replication-base
/// convention).
fn follow_new_primary(
    replication: &ReplicationState,
    member_table: &[(String, String, u16)],
    pid: &str,
) {
    let Some((_, host, cport)) = member_table.iter().find(|(id, _, _)| id == pid) else {
        eprintln!("kevy: elect — primary '{pid}' not in the member table; not retargeting");
        return;
    };
    let upstream = format!("{host}:{}", cport + 10_000);
    match crate::replication::retarget_upstream(replication, &upstream) {
        Ok(()) => {
            eprintln!("kevy: elect — following new primary '{pid}' at {upstream}");
            if kevy_rt::repl_trace() {
                kevy_rt::repl_trace_line(format_args!(
                    "elect: runners respawned toward {upstream} with reset cursors"
                ));
            }
        }
        Err(e) => eprintln!("kevy: elect — retarget to '{pid}' ({upstream}) failed: {e}"),
    }
}

/// Build the `Elector` for this node from config: identity, peer
/// set, advertised address — and the durability backend
/// (`<data_dir>/elect.meta` via [`crate::elect_persist::
/// FileElectorPersist`]), whose `load` restores the persisted
/// `(epoch, votedFor)` so a restarted node never double-votes or
/// reuses a consumed epoch.
fn build_elector(
    cfg: &Config,
    elect_cfg: ElectConfig,
    replication: &ReplicationState,
) -> (Elector, Role) {
    let persist = crate::elect_persist::FileElectorPersist::new(&cfg.server.data_dir);
    let mut start_role = match cfg.replication.role {
        ReplicationRole::Primary => Role::Primary,
        _ => Role::Replica,
    };
    // Role clamp: in an elect quorum, the config role is an initial
    // PREFERENCE — write authority comes only from the election (win →
    // the topology callback opens writes). The original clamp keyed on
    // a persisted epoch > 0, but a first-generation primary that dies
    // BEFORE any election restarts with an empty elect.meta and would
    // come back writable into a cluster that has since chosen someone
    // else (fork). Cold-start cost of the unconditional clamp is one
    // election round.
    if matches!(start_role, Role::Primary) {
        let epoch = kevy_elect::ElectorPersist::load(&persist).0;
        eprintln!(
            "kevy: elect — quorum config: starting read-only until elected (persisted epoch {epoch})"
        );
        start_role = Role::Replica;
        replication.set_read_only(true);
        replication.force_replica_flag();
    }
    let peer_ids: Vec<String> = cfg
        .cluster
        .peers
        .iter()
        .map(|p| p.node_id.clone())
        .collect();
    let advertised_addr = format!("{}:{}", advertised_host(cfg), cfg.server.port);
    let elector = Elector::new(
        cfg.cluster.node_id.clone(),
        peer_ids,
        advertised_addr,
        start_role,
        elect_cfg,
        ElectJitter::System,
    )
    .with_persist(Box::new(persist));
    (elector, start_role)
}

fn resolved_elect_port_base(cfg: &Config) -> u16 {
    if cfg.cluster.elect_port_base != 0 {
        return cfg.cluster.elect_port_base;
    }
    // Default: `server.port + 200` so it doesn't collide with the
    // replication listener (server.port + 10000 by default) or the
    // cluster port (server.port + 1 in cluster mode).
    cfg.server.port.saturating_add(200)
}

fn advertised_host(cfg: &Config) -> String {
    // Use the bind address as the advertised host. Operators behind
    // NAT will want to set an external IP via a future config knob —
    // if the bind is 0.0.0.0 (all interfaces), the advertised string
    // is still 0.0.0.0 (caller-resolved by the peer's hostname
    // mapping).
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

    #[test]
    fn set_view_offset_out_of_range_shard_is_safe() {
        // A runtime built with more shards than the state was sized
        // for must not panic; the extra shard's offset is dropped.
        let e = ElectionState::new(2);
        let r = ReplicationState::new(2, false, 0);
        e.set_view_offset(&r, 7, 100);
        assert_eq!(e.aggregate_offset(), 0);
        e.set_view_offset(&r, 1, 40);
        assert_eq!(e.aggregate_offset(), 40);
    }
}
