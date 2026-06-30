//! `ROLE` — operational surface for the primary/replica topology.
//!
//! The per-tick `master_repl_offset` + `connected_replicas` count is
//! stashed in [`REPLICATION_VIEW`] by `KevyCommands::on_replication_view`
//! (driven from `kevy_rt::Shard::tick_replication_view`). `ROLE` reads
//! that thread-local + `config.replication.role` and emits the
//! Redis-shaped reply.
//!
//! Current simplifications:
//! - Per-replica IP / port / offset entries in the master reply are
//!   intentionally empty. Capturing replica peer addresses needs a
//!   `getpeername` FFI binding in `kevy-sys` + threading it through
//!   `accept_ready_replication`; tracked as a follow-up alongside
//!   T1.31 (`INFO replication` enrichment).
//! - Replica-side status is always `"connect"` (Redis's "configured,
//!   not yet connecting" state) — the v1.18 replica role is config-
//!   declared but the active upstream link only lands at T1.29 /
//!   T1.30 (`REPLICAOF`).
//! - The view reflects the *answering shard*. Multi-shard aggregation
//!   (sum offsets across all shards) is a follow-up — for now, ROLE
//!   reports per-shard, which is correct for a sharded primary
//!   (each shard streams its own keyspace slice independently).

use std::cell::RefCell;
use std::net::Ipv4Addr;

use kevy_config::ReplicationRole;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_simple_string};

use crate::config_global;

use super::wrong_args;

/// Live replication view stashed by `KevyCommands::on_replication_view`.
#[derive(Clone, Default)]
pub(crate) struct ReplicationView {
    pub(crate) master_repl_offset: u64,
    /// Per-replica `(ipv4, port, sent_offset)` triple — populated
    /// by `kevy_rt::Shard::tick_replication_view` (T1.28.5).
    pub(crate) replicas: Vec<(Ipv4Addr, u16, u64)>,
}

thread_local! {
    /// Per-tick replication view. Stale by at most one tick interval
    /// (default 100 ms). Empty (all-default) when this shard has no
    /// `ReplicationSource` installed (publisher early returns; cell
    /// keeps the construction default).
    static REPLICATION_VIEW: RefCell<ReplicationView> = RefCell::new(ReplicationView::default());
}

/// Record the answering shard's replication view (see [`REPLICATION_VIEW`]).
pub(crate) fn set_replication_view(
    master_repl_offset: u64,
    replicas: Vec<(Ipv4Addr, u16, u64)>,
) {
    REPLICATION_VIEW.with(|c| {
        *c.borrow_mut() = ReplicationView {
            master_repl_offset,
            replicas,
        };
    });
}

/// Read the answering shard's replication view. Returns a default
/// (offset=0, no replicas) when replication is off on this shard.
pub(crate) fn replication_view() -> ReplicationView {
    REPLICATION_VIEW.with(|c| c.borrow().clone())
}

/// `ROLE` — see <https://redis.io/commands/role/>. v1.18 mapping:
///
/// - master (standalone / primary, OR replica with no active runner) →
///   `["master", <offset>, []]` (per-replica array intentionally empty;
///   offset is this shard's `next_offset` at the most recent tick).
/// - replica (any time a runner is live — set by `REPLICAOF host port`
///   or by startup `role = "replica"`) → `["slave", <host>, <port>,
///   "connect", 0]` (host/port from the live upstream slot; status is
///   `"connect"` — a richer status would require runner→view feedback
///   tracked as a T1.31 follow-up).
///
/// Live state wins over startup config: a server that started as
/// `standalone` but ran `REPLICAOF` later reports `slave` until
/// `REPLICAOF NO ONE`.
pub(crate) fn cmd_role<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 1 {
        return wrong_args(out, "role");
    }
    // v3-cluster Phase 1.5: kevy-elect's live view wins over both
    // dynamic REPLICAOF and static config when the operator
    // configured `[cluster] peers = "..."`. Otherwise fall through
    // to the v1.18 logic (REPLICAOF state → static config).
    if let Some(snap) = crate::elect_integration::current_snapshot() {
        use kevy_elect::message::Role as ElectRole;
        match snap.role {
            ElectRole::Primary => return emit_master(out),
            ElectRole::Replica | ElectRole::Candidate => {
                // Use the elector's current_primary as the upstream
                // address-string; v1.19 advertises `host:port` of
                // the kevy compat port in `ANNOUNCE`, so the
                // primary id resolves to a parseable addr.
                let (host, port) = match snap.current_primary.as_deref() {
                    Some(_addr_or_id) => current_primary_host_port_from_config(),
                    None => ("".to_string(), 0),
                };
                return emit_replica_addr(&host, port, out);
            }
        }
    }
    // Live state from `replica_state` wins over the static config —
    // dynamic REPLICAOF retarget at runtime is the source of truth.
    if let Some((host, port)) = crate::replica_state::current_upstream() {
        let host_str = host.to_string();
        return emit_replica_addr(&host_str, port, out);
    }
    let cfg = config_global::get();
    match cfg.replication.role {
        ReplicationRole::Standalone | ReplicationRole::Primary => emit_master(out),
        ReplicationRole::Replica => emit_replica(cfg.replication.upstream.as_deref(), out),
    }
}

/// Walk the configured peer list for the primary node's
/// host/port. Used by `cmd_role` when kevy-elect names a primary
/// id and we need to render it as `host:port` for the Redis
/// reply. Falls back to `("", 0)` when the elector's
/// `current_primary` doesn't match any peer in the config (the
/// peer list and ANNOUNCE addr should agree, but defensive).
fn current_primary_host_port_from_config() -> (String, u16) {
    let snap = match crate::elect_integration::current_snapshot() {
        Some(s) => s,
        None => return (String::new(), 0),
    };
    let Some(pid) = snap.current_primary else {
        return (String::new(), 0);
    };
    let cfg = config_global::get();
    for p in &cfg.cluster.peers {
        if p.node_id == pid {
            return (p.host.clone(), p.port);
        }
    }
    (String::new(), 0)
}

/// `REPLICAOF host port` / `REPLICAOF NO ONE` (T1.29.5 / T1.30).
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
/// — for an embedded process — `kevy::serve` never installed the
/// per-shard senders).
///
/// Side effects are global (process-level): every connected client
/// sees the same effect — there is no per-connection retarget.
pub(crate) fn cmd_replicaof<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 3 {
        return wrong_args(out, "replicaof");
    }
    let arg1 = &args[1];
    let arg2 = &args[2];
    // REPLICAOF NO ONE — demote.
    if arg1.eq_ignore_ascii_case(b"NO") && arg2.eq_ignore_ascii_case(b"ONE") {
        crate::replication::demote_to_standalone();
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
    if let Err(reason) = crate::replication::retarget_upstream(&upstream) {
        return encode_error(out, &format!("ERR {reason}"));
    }
    encode_simple_string(out, "OK");
}

fn emit_master(out: &mut Vec<u8>) {
    let view = replication_view();
    encode_array_len(out, 3);
    encode_bulk(out, b"master");
    encode_integer(out, view.master_repl_offset as i64);
    // T1.28.5: inner per-replica list now populated with
    // `(ip, port, sent_offset)` triples. Redis encodes the port +
    // offset as **bulk strings** (not integers) — matches the shape
    // most clients (incl. redis-rs) parse against.
    encode_array_len(out, view.replicas.len() as i64);
    for (ip, port, offset) in &view.replicas {
        let ip_str = ip.to_string();
        let port_str = port.to_string();
        let off_str = offset.to_string();
        encode_array_len(out, 3);
        encode_bulk(out, ip_str.as_bytes());
        encode_bulk(out, port_str.as_bytes());
        encode_bulk(out, off_str.as_bytes());
    }
}

fn emit_replica(upstream: Option<&str>, out: &mut Vec<u8>) {
    let (host, port) = parse_upstream(upstream);
    emit_replica_addr(host, port, out);
}

fn emit_replica_addr(host: &str, port: u16, out: &mut Vec<u8>) {
    encode_array_len(out, 5);
    encode_bulk(out, b"slave");
    encode_bulk(out, host.as_bytes());
    encode_integer(out, i64::from(port));
    encode_bulk(out, b"connect");
    encode_integer(out, 0);
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

    // Serialise the unit tests that touch process-global replica
    // state — see [`crate::replica_state::TEST_STATE_GUARD`] for the
    // mutex.

    fn run(offset: u64, replica_count: usize) -> Vec<u8> {
        let _g = crate::replica_state::TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Ensure ROLE's `current_upstream` path returns `None` for
        // this test — defensive against sibling tests that may have
        // started a runner.
        crate::replica_state::stop_runners();
        let replicas: Vec<_> = (0..replica_count)
            .map(|i| (Ipv4Addr::new(10, 0, 0, (i + 1) as u8), 6004, offset))
            .collect();
        set_replication_view(offset, replicas);
        let mut a = Argv::default();
        a.push(b"ROLE");
        let mut out = Vec::new();
        cmd_role(&a, &mut out);
        out
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
        // T1.28.5: with 2 replicas in the view, ROLE emits the
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
        cmd_role(&a, &mut out);
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

    fn replicaof(args: &[&[u8]]) -> Vec<u8> {
        let _g = crate::replica_state::TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::replica_state::stop_runners();
        let mut a = Argv::default();
        a.push(b"REPLICAOF");
        for arg in args {
            a.push(arg);
        }
        let mut out = Vec::new();
        cmd_replicaof(&a, &mut out);
        // Defensive cleanup: any runner the command spawned should
        // not outlive this test — it would race ROLE tests below.
        crate::replica_state::stop_runners();
        out
    }

    #[test]
    fn replicaof_host_port_returns_ok() {
        // Install N=1 sender so the retarget can spawn a runner; the
        // runner will fail to connect to localhost:6379 (nothing
        // listening) but the command returns +OK as soon as the
        // runner is spawned. `replicaof()` calls `stop_runners`
        // around the invocation so the runner is gone by the time
        // the next test runs.
        let _g = crate::replica_state::TEST_STATE_GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tx, _rx) = kevy_rt::replica_inbox_pair();
        crate::replica_state::install_senders(vec![tx]);
        drop(_g); // release the guard so `replicaof()` can re-acquire
        assert_eq!(replicaof(&[b"127.0.0.1", b"6379"]), b"+OK\r\n");
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
