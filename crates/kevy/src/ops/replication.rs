//! `ROLE` — operational surface for the primary/replica topology.
//!
//! Each shard's per-tick `master_repl_offset` + per-conn replica rows
//! land in two places via `KevyCommands::on_replication_view` (driven
//! from `kevy_rt::Shard::tick_replication_view`): the shard zone
//! ([`crate::state::ShardCtx`], a [`ReplicationView`], serving
//! per-shard consumers like the min-replicas gate) and a shared
//! per-shard slot in [`crate::state::ObsState`]. `ROLE` and `INFO
//! replication` fold every slot into the instance-wide answer
//! ([`aggregate_replication`]): offsets sum across shards, and the
//! per-stream rows of one replica process (grouped by the
//! `<process>#<stream>` id shape) merge into a single entry.
//!
//! Replica-side status is live: `sync` during a full-resync snapshot
//! ship, `connected` on a fresh upstream heartbeat, `connect`
//! otherwise; the reported offset is the instance-wide applied sum.

use std::net::Ipv4Addr;

use kevy_config::ReplicationRole;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_simple_string};

use crate::state::Ctx;

use super::wrong_args;

/// Live replication view stashed in the shard zone by
/// `KevyCommands::on_replication_view`. Stale by at most one tick
/// interval (default 100 ms); all-default when this shard has no
/// `ReplicationSource` installed.
#[derive(Clone, Default)]
pub(crate) struct ReplicationView {
    /// Per-replica `(id, ipv4, port, sent_offset, ack)` — populated by
    /// `kevy_rt::Shard::tick_replication_view`. `ack` carries the
    /// acked offset plus the ACK's age for the min-replicas lag gate
    /// (the shard-zone copy's only consumer — instance-wide reporting
    /// reads the [`crate::state::ObsState`] slots instead).
    pub(crate) replicas: Vec<kevy_rt::ReplicaViewRow>,
}

/// One replica process folded across every per-shard stream: the rows
/// sharing an id prefix (`<process>#<stream>`) sum their offsets, and
/// the aggregate only counts as acked when every stream has a real ACK.
pub(crate) struct AggReplica {
    pub(crate) ip: Ipv4Addr,
    /// The client port the replica advertised in its id — a truthful
    /// "where to reach it", unlike the per-conn ephemeral source port.
    /// Falls back to the first conn's peer port for foreign id shapes.
    pub(crate) port: u16,
    pub(crate) sent: u64,
    pub(crate) acked: Option<u64>,
}

/// Fold every shard's replication view into the instance-wide answer:
/// `(master_repl_offset_sum, one AggReplica per replica process)`.
/// Offsets sum across shards — the same convention as the election
/// offset sum, and equal to a caught-up replica's own applied sum.
pub(crate) fn aggregate_replication(
    views: &[crate::state::ReplShardView],
) -> (u64, Vec<AggReplica>) {
    let offset_sum = views.iter().fold(0u64, |a, v| a.saturating_add(v.offset));
    let mut keys: Vec<String> = Vec::new();
    let mut reps: Vec<(AggReplica, bool)> = Vec::new();
    for view in views {
        for (id, ip, peer_port, sent, ack) in &view.replicas {
            let process = id.split('#').next().unwrap_or(id);
            let slot = match keys.iter().position(|k| k == process) {
                Some(i) => i,
                None => {
                    keys.push(process.to_string());
                    reps.push((
                        AggReplica {
                            ip: *ip,
                            port: advertised_port(process).unwrap_or(*peer_port),
                            sent: 0,
                            acked: Some(0),
                        },
                        true,
                    ));
                    reps.len() - 1
                }
            };
            let (agg, all_acked) = &mut reps[slot];
            agg.sent = agg.sent.saturating_add(*sent);
            match ack {
                Some(a) if *all_acked => {
                    agg.acked = agg.acked.map(|v| v.saturating_add(a.acked_offset));
                }
                _ => {
                    *all_acked = false;
                    agg.acked = None;
                }
            }
        }
    }
    (offset_sum, reps.into_iter().map(|(agg, _)| agg).collect())
}

/// Extract the advertised client port from a kevy replica-id process
/// prefix (`kevy-replica-<port>`).
fn advertised_port(process: &str) -> Option<u16> {
    process.rsplit('-').next()?.parse().ok()
}

/// `ROLE` — see <https://redis.io/commands/role/>. Mapping:
///
/// - master (standalone / primary, OR replica with no active runner) →
///   `["master", <offset>, [(ip, port, offset)…]]` — instance-wide:
///   the offset sums every shard's stream position and the list holds
///   one entry per replica process (port = its advertised client
///   port, offset = its summed acked position).
/// - replica (any time a runner is live — set by `REPLICAOF host port`
///   or by startup `role = "replica"`) → `["slave", <host>, <port>,
///   <state>, <offset>]` — host/port from the live upstream slot,
///   state from the live link (`sync` during a full-resync ship /
///   `connected` on a fresh heartbeat / `connect` otherwise), offset
///   = the instance-wide applied sum.
///
/// Live state wins over startup config: a server that started as
/// `standalone` but ran `REPLICAOF` later reports `slave` until
/// `REPLICAOF NO ONE`.
pub(crate) fn cmd_role<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 1 {
        return wrong_args(out, "role");
    }
    // kevy-elect's live view wins over both
    // dynamic REPLICAOF and static config when the operator
    // configured `[cluster] peers = "..."`. Otherwise fall through
    // to the REPLICAOF-state → static-config precedence below.
    if let Some(snap) = ctx.state.election.current_snapshot() {
        use kevy_elect::message::Role as ElectRole;
        match snap.role {
            ElectRole::Primary => return emit_master(ctx, out),
            ElectRole::Replica | ElectRole::Candidate => {
                // Use the elector's current_primary as the upstream
                // address-string; `ANNOUNCE` advertises `host:port`
                // of the kevy compat port, so the
                // primary id resolves to a parseable addr.
                let (host, port) = match snap.current_primary.as_deref() {
                    Some(_addr_or_id) => current_primary_host_port_from_config(ctx),
                    None => ("".to_string(), 0),
                };
                return emit_replica_addr(ctx, &host, port, out);
            }
        }
    }
    // Live replication state wins over the static config — dynamic
    // REPLICAOF retarget at runtime is the source of truth.
    if let Some((host, port)) = ctx.state.replication.current_upstream() {
        let host_str = host.to_string();
        return emit_replica_addr(ctx, &host_str, port, out);
    }
    let cfg = ctx.state.config();
    match cfg.replication.role {
        ReplicationRole::Standalone | ReplicationRole::Primary => emit_master(ctx, out),
        ReplicationRole::Replica => emit_replica(ctx, cfg.replication.upstream.as_deref(), out),
    }
}

/// Walk the configured peer list for the primary node's
/// host/port. Used by `cmd_role` when kevy-elect names a primary
/// id and we need to render it as `host:port` for the Redis
/// reply. Falls back to `("", 0)` when the elector's
/// `current_primary` doesn't match any peer in the config (the
/// peer list and ANNOUNCE addr should agree, but defensive).
fn current_primary_host_port_from_config(ctx: &Ctx<'_>) -> (String, u16) {
    let snap = match ctx.state.election.current_snapshot() {
        Some(s) => s,
        None => return (String::new(), 0),
    };
    let Some(pid) = snap.current_primary else {
        return (String::new(), 0);
    };
    let cfg = ctx.state.config();
    for p in &cfg.cluster.peers {
        if p.node_id == pid {
            return (p.host.clone(), p.port);
        }
    }
    (String::new(), 0)
}

/// `REPLICAOF host port` / `REPLICAOF NO ONE`.
///
/// Parses + validates argv, then:
/// - `NO ONE` → [`crate::replication::demote_to_standalone`] (stops
///   every active runner thread, clears the live upstream slot).
/// - `host port` → [`crate::replication::retarget_upstream`] (stops
///   any prior fleet, resolves the host, spawns a new per-shard
///   runner fleet pointing at `(host, port + shard_id)`).
///
/// Replies `+OK` on success, `-ERR <reason>` on parse / resolve
/// failure (host empty, port out of range, host not resolvable, or
/// — for an embedded process — the replica inboxes were never wired
/// into a runtime).
///
/// Side effects are server-wide: every connected client sees the
/// same effect — there is no per-connection retarget.
pub(crate) fn cmd_replicaof<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 3 {
        return wrong_args(out, "replicaof");
    }
    let arg1 = &args[1];
    let arg2 = &args[2];
    // REPLICAOF NO ONE — demote.
    if arg1.eq_ignore_ascii_case(b"NO") && arg2.eq_ignore_ascii_case(b"ONE") {
        crate::replication::demote_to_standalone(&ctx.state.replication);
        encode_simple_string(out, "OK");
        return;
    }
    // REPLICAOF host port — validate then retarget.
    let Ok(port_str) = std::str::from_utf8(arg2) else {
        return encode_error(out, "ERR value is not an integer or out of range");
    };
    let Ok(port): Result<u16, _> = port_str.parse() else {
        return encode_error(out, "ERR value is not an integer or out of range");
    };
    let Ok(host_str) = std::str::from_utf8(arg1) else {
        return encode_error(out, "ERR Invalid master host");
    };
    if host_str.is_empty() {
        return encode_error(out, "ERR Invalid master host");
    }
    let upstream = format!("{host_str}:{port}");
    if let Err(reason) = crate::replication::retarget_upstream(&ctx.state.replication, &upstream) {
        return encode_error(out, &format!("ERR {reason}"));
    }
    encode_simple_string(out, "OK");
}

fn emit_master(ctx: &Ctx<'_>, out: &mut Vec<u8>) {
    // Instance-wide truth: fold every shard's view (offset sum, one
    // entry per replica process).
    let views = ctx.state.obs.repl_views();
    let (offset, replicas) = aggregate_replication(&views);
    encode_array_len(out, 3);
    encode_bulk(out, b"master");
    encode_integer(out, offset as i64);
    // The inner per-replica list carries
    // `(ip, port, offset)` triples. Redis encodes the port +
    // offset as **bulk strings** (not integers) — matches the shape
    // most clients (incl. redis-rs) parse against.
    encode_array_len(out, replicas.len() as i64);
    // ROLE reports the replica's ACKED offset when every stream has
    // one (real acknowledgment), falling back to sent (pre-first-ACK).
    for agg in &replicas {
        let ip_str = agg.ip.to_string();
        let port_str = agg.port.to_string();
        let off_str = agg.acked.unwrap_or(agg.sent).to_string();
        encode_array_len(out, 3);
        encode_bulk(out, ip_str.as_bytes());
        encode_bulk(out, port_str.as_bytes());
        encode_bulk(out, off_str.as_bytes());
    }
}

fn emit_replica(ctx: &Ctx<'_>, upstream: Option<&str>, out: &mut Vec<u8>) {
    let (host, port) = parse_upstream(upstream);
    emit_replica_addr(ctx, host, port, out);
}

fn emit_replica_addr(ctx: &Ctx<'_>, host: &str, port: u16, out: &mut Vec<u8>) {
    // Live link truth: `sync` while a full-resync snapshot ship is in
    // flight, `connected` on a fresh heartbeat, `connect` otherwise —
    // the same sources INFO replication reports. The offset is the
    // instance-wide applied sum (comparable with the primary's
    // per-shard offset sum).
    let repl = &ctx.state.replication;
    let state: &[u8] = if repl.loading() {
        b"sync"
    } else if repl.replica_link_view().0 {
        b"connected"
    } else {
        b"connect"
    };
    let offset = repl.applied_offset_sum();
    encode_array_len(out, 5);
    encode_bulk(out, b"slave");
    encode_bulk(out, host.as_bytes());
    encode_integer(out, i64::from(port));
    encode_bulk(out, state);
    encode_integer(out, offset as i64);
}

/// Parse `"host:port"` into `(host, port)`. Tolerates missing port
/// (returns `0`) and an empty / `None` upstream (returns `("", 0)`).
/// IPv6 literals can be bracketed (`[::1]:7000`); the rightmost `:`
/// after the closing `]` is the port separator.
fn parse_upstream(s: Option<&str>) -> (&str, u16) {
    let Some(s) = s else { return ("", 0) };
    let (host, port_str) = match s.rfind(':') {
        Some(idx) => (&s[..idx], &s[idx + 1..]),
        None => return (s, 0),
    };
    let port: u16 = port_str.parse().unwrap_or(0);
    (host, port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn run(offset: u64, replica_count: usize) -> Vec<u8> {
        // Distinct process prefixes (one advertised port per replica)
        // so aggregation keeps them as separate entries.
        let replicas: Vec<_> = (0..replica_count)
            .map(|i| {
                (
                    format!("kevy-replica-{}#0", 6004 + i),
                    Ipv4Addr::new(10, 0, 0, (i + 1) as u8),
                    50_000 + i as u16,
                    offset,
                    Some(kevy_rt::ReplicaAck { acked_offset: offset, ack_age_ms: 0 }),
                )
            })
            .collect();
        let c = crate::KevyCommands::new();
        c.state()
            .obs
            .publish_repl_view(0, crate::state::ReplShardView { offset, replicas });
        let mut a = Argv::default();
        a.push(b"ROLE");
        let mut out = Vec::new();
        cmd_role(&c.ctx(), &a, &mut out);
        out
    }

    #[test]
    fn aggregate_folds_one_process_across_shards() {
        let ip = Ipv4Addr::new(10, 0, 0, 9);
        let ack = |off| Some(kevy_rt::ReplicaAck { acked_offset: off, ack_age_ms: 0 });
        let row = |shard: usize, acked| {
            (format!("kevy-replica-7391#{shard}"), ip, 40_000 + shard as u16, 10u64, acked)
        };
        let views = vec![
            crate::state::ReplShardView { offset: 100, replicas: vec![row(0, ack(10))] },
            crate::state::ReplShardView { offset: 40, replicas: vec![row(1, ack(7))] },
        ];
        let (offset, reps) = aggregate_replication(&views);
        assert_eq!(offset, 140, "offsets sum across shards");
        assert_eq!(reps.len(), 1, "one entry per replica process");
        assert_eq!(reps[0].port, 7391, "advertised client port, not the peer port");
        assert_eq!(reps[0].acked, Some(17), "acked sums across streams");
        assert_eq!(reps[0].sent, 20);
    }

    #[test]
    fn aggregate_reports_syncing_when_any_stream_lacks_an_ack() {
        let ip = Ipv4Addr::new(10, 0, 0, 9);
        let ack = Some(kevy_rt::ReplicaAck { acked_offset: 5, ack_age_ms: 0 });
        let views = vec![
            crate::state::ReplShardView {
                offset: 10,
                replicas: vec![("kevy-replica-7391#0".into(), ip, 1, 5, ack)],
            },
            crate::state::ReplShardView {
                offset: 10,
                replicas: vec![("kevy-replica-7391#1".into(), ip, 2, 5, None)],
            },
        ];
        let (_, reps) = aggregate_replication(&views);
        assert_eq!(reps.len(), 1);
        assert_eq!(reps[0].acked, None, "one un-ACKed stream marks the process syncing");
    }

    #[test]
    fn aggregate_falls_back_to_peer_port_for_foreign_ids() {
        let ip = Ipv4Addr::new(10, 0, 0, 9);
        let views = vec![crate::state::ReplShardView {
            offset: 1,
            replicas: vec![("some-other-agent".into(), ip, 41_234, 1, None)],
        }];
        let (_, reps) = aggregate_replication(&views);
        assert_eq!(reps[0].port, 41_234, "unparseable id shape falls back to the peer port");
    }

    #[test]
    fn role_default_master_zero_offset() {
        // Default config = standalone, no replication, no replicas.
        let out = run(0, 0);
        assert_eq!(out, b"*3\r\n$6\r\nmaster\r\n:0\r\n*0\r\n");
    }

    #[test]
    fn role_master_offset_reflects_view() {
        // Offset reflects the view; per-replica list is empty here
        // (count=0).
        let out = run(12345, 0);
        assert_eq!(out, b"*3\r\n$6\r\nmaster\r\n:12345\r\n*0\r\n");
    }

    #[test]
    fn role_master_emits_per_replica_array() {
        // With 2 replicas in the view, ROLE emits the
        // inner array with `(ip, port, offset)` triples — each as
        // bulk strings (Redis convention).
        let out = run(12345, 2);
        let s = String::from_utf8(out).unwrap();
        // Outer array of 3: master / offset / inner-array
        assert!(s.starts_with("*3\r\n$6\r\nmaster\r\n:12345\r\n"), "got: {s}");
        // Inner array of 2 entries
        assert!(s.contains("*2\r\n*3\r\n"), "expected inner *2 then *3 per entry; got: {s}");
        // Each entry's IP from the test helper's series
        assert!(s.contains("10.0.0.1"), "got: {s}");
        assert!(s.contains("10.0.0.2"), "got: {s}");
    }

    #[test]
    fn role_wrong_args_returns_error() {
        let mut a = Argv::default();
        a.push(b"ROLE");
        a.push(b"extra");
        let mut out = Vec::new();
        let c = crate::KevyCommands::new();
        cmd_role(&c.ctx(), &a, &mut out);
        assert!(out.starts_with(b"-ERR"));
    }

    #[test]
    fn parse_upstream_host_port() {
        assert_eq!(parse_upstream(Some("127.0.0.1:6379")), ("127.0.0.1", 6379));
    }

    #[test]
    fn parse_upstream_missing_port_defaults_to_zero() {
        assert_eq!(parse_upstream(Some("primary.local")), ("primary.local", 0));
    }

    #[test]
    fn parse_upstream_none_yields_empty() {
        assert_eq!(parse_upstream(None), ("", 0));
    }

    #[test]
    fn parse_upstream_ipv6_uses_rightmost_colon() {
        assert_eq!(parse_upstream(Some("[::1]:7000")), ("[::1]", 7000));
    }

    fn replicaof_on(c: &crate::KevyCommands, args: &[&[u8]]) -> Vec<u8> {
        let mut a = Argv::default();
        a.push(b"REPLICAOF");
        for arg in args {
            a.push(arg);
        }
        let mut out = Vec::new();
        cmd_replicaof(&c.ctx(), &a, &mut out);
        out
    }

    fn replicaof(args: &[&[u8]]) -> Vec<u8> {
        replicaof_on(&crate::KevyCommands::new(), args)
    }

    #[test]
    fn replicaof_host_port_returns_ok() {
        // Wire the state's inbox receivers (kept alive for the test)
        // so the retarget can spawn a runner; the runner will fail to
        // connect to localhost:6379 (nothing listening) but the
        // command returns +OK as soon as the runner is spawned.
        let c = crate::KevyCommands::new();
        let _receivers = c.state().take_replica_inboxes().expect("first take");
        assert_eq!(replicaof_on(&c, &[b"127.0.0.1", b"6379"]), b"+OK\r\n");
        // Stop the runner before the receivers drop.
        c.state().replication.stop_runners();
    }

    #[test]
    fn replicaof_no_one_returns_ok() {
        // NO ONE doesn't need senders — it just stops runners (no-op
        // when none).
        assert_eq!(replicaof(&[b"NO", b"ONE"]), b"+OK\r\n");
        assert_eq!(replicaof(&[b"no", b"one"]), b"+OK\r\n");
        assert_eq!(replicaof(&[b"No", b"OnE"]), b"+OK\r\n");
    }

    #[test]
    fn replicaof_wrong_args_errors() {
        assert!(replicaof(&[]).starts_with(b"-ERR"));
        assert!(replicaof(&[b"primary"]).starts_with(b"-ERR"));
        assert!(replicaof(&[b"a", b"b", b"c"]).starts_with(b"-ERR"));
    }

    #[test]
    fn replicaof_bad_port_errors() {
        assert!(replicaof(&[b"primary", b"not-a-number"]).starts_with(b"-ERR"));
        assert!(replicaof(&[b"primary", b"99999"]).starts_with(b"-ERR"));
        assert!(replicaof(&[b"primary", b"-1"]).starts_with(b"-ERR"));
    }

    #[test]
    fn replicaof_empty_host_errors() {
        assert!(replicaof(&[b"", b"6379"]).starts_with(b"-ERR"));
    }
}
