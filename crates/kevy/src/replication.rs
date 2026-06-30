//! Bridge between `kevy-config`'s `[replication]` section and
//! `kevy-rt`'s `Runtime` builder. Split out of `lib.rs` to keep that
//! file under the 500-LOC house rule.

use std::net::{IpAddr, Ipv4Addr};

use kevy_config::{Config, ReplicationRole};
use kevy_rt::{Commands, Runtime, replica_inbox_pair};

use crate::replica_state;

/// Resolved replication listener base port: `[replication].listen_port_base`,
/// or `server.port + 10000` when left at the `0` default. Shard `i`
/// listens at this + `i` (per Issue Ledger I2 — per-shard listener).
/// `saturating_add` matches the cluster-port helper; `Runtime::run`
/// rejects the (base, nshards) range loudly when it would wrap u16.
pub(crate) fn replication_port_base(cfg: &Config) -> u16 {
    match cfg.replication.listen_port_base {
        0 => cfg.server.port.saturating_add(10_000),
        base => base,
    }
}

/// Apply the `[replication]` section to a [`Runtime`] under construction.
///
/// **Always** installs per-shard replica inboxes — even when
/// `role = standalone` or `primary` — so that a runtime-issued
/// `REPLICAOF host port` (T1.29.5) can spawn runners that push into
/// the inboxes without changing the already-running shards. The
/// senders go into [`crate::replica_state::install_senders`]; the
/// receivers flow to `Runtime::with_replica_inboxes`. Standalone
/// shards do one extra `Option::is_some` check per tick (empty
/// channel, immediate return) — measurable at 0 ns vs. the existing
/// `tick_persist` cost.
///
/// `role = primary` additionally wires the downstream backlog +
/// listener. `role = replica` with a valid `upstream` additionally
/// spawns the initial runner fleet via
/// [`crate::replica_state::start_runners`].
pub(crate) fn apply<C: Commands>(
    runtime: Runtime<C>,
    cfg: &Config,
    nshards: usize,
) -> Runtime<C> {
    // Per-shard inbox pairs — always allocated.
    let mut senders = Vec::with_capacity(nshards);
    let mut receivers = Vec::with_capacity(nshards);
    for _ in 0..nshards {
        let (tx, rx) = replica_inbox_pair();
        senders.push(tx);
        receivers.push(rx);
    }
    replica_state::install_senders(senders);
    let runtime = runtime.with_replica_inboxes(receivers);

    match cfg.replication.role {
        ReplicationRole::Primary => runtime
            .with_replication(true, cfg.replication.replication_buffer_size)
            .with_replication_listener(replication_port_base(cfg))
            .with_replication_reconnect_window(cfg.replication.reconnect_window_ms),
        ReplicationRole::Replica => {
            spawn_initial_runners_from_config(cfg);
            runtime
        }
        ReplicationRole::Standalone => runtime,
    }
}

/// Bring up the initial runner fleet for the `[replication] role =
/// "replica"` startup path. Misconfig (unset / unparseable /
/// unresolvable upstream) logs a `kevy:` warning and leaves the
/// runtime in standalone-effective mode — admins can fix the config
/// + REPLICAOF without restart once T1.29.5 ships.
fn spawn_initial_runners_from_config(cfg: &Config) {
    let Some(upstream) = cfg.replication.upstream.as_deref() else {
        eprintln!(
            "kevy: [replication] role = \"replica\" but upstream is unset; \
             no runners spawned — use REPLICAOF host port to set one"
        );
        return;
    };
    let Some((host_str, port_base)) = parse_upstream(upstream) else {
        eprintln!(
            "kevy: [replication] upstream {upstream:?} not parseable as host:port; \
             no runners spawned"
        );
        return;
    };
    let Some(host) = resolve_host(&host_str) else {
        eprintln!(
            "kevy: [replication] upstream host {host_str:?} not resolvable; \
             no runners spawned"
        );
        return;
    };
    if let Err(e) = replica_state::start_runners((host, port_base)) {
        eprintln!("kevy: start_runners failed: {e}");
    }
}

/// Public retarget entry point used by `REPLICAOF host port` (T1.29.5).
/// Parses + resolves + starts the new fleet (stopping any prior).
/// Returns `Err(static reason)` on parse / resolve failure so the
/// command can map it to a `-ERR` reply.
pub(crate) fn retarget_upstream(upstream: &str) -> Result<(), &'static str> {
    let (host_str, port_base) = parse_upstream(upstream).ok_or("upstream not host:port")?;
    let host = resolve_host(&host_str).ok_or("upstream host not resolvable")?;
    replica_state::start_runners((host, port_base))
}

/// Public demote entry point used by `REPLICAOF NO ONE` (T1.30).
/// Stops every active runner and clears the upstream slot.
pub(crate) fn demote_to_standalone() {
    replica_state::stop_runners();
}

/// Parse `"host:port"`. Tolerates IPv6 brackets (`[::1]:7000`).
pub(crate) fn parse_upstream(s: &str) -> Option<(String, u16)> {
    let idx = s.rfind(':')?;
    let host = &s[..idx];
    let port: u16 = s[idx + 1..].parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Resolve a host string to one `IpAddr`. Accepts dotted IPv4
/// literals (the v1.18 minimum); DNS / IPv6 are best-effort via the
/// std `to_socket_addrs` path. The rarely-used IPv6 bracketed form
/// arrives stripped of brackets here (caller strips `[…]`).
pub(crate) fn resolve_host(host: &str) -> Option<IpAddr> {
    let host = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }
    // Fallback for hostnames — std::net::ToSocketAddrs needs a port;
    // dial with `:0` and take the first answer's IP.
    use std::net::ToSocketAddrs;
    (host, 0u16)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
        .map(|s| s.ip())
        .or(Some(IpAddr::V4(Ipv4Addr::LOCALHOST)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_upstream_host_port() {
        assert_eq!(
            parse_upstream("127.0.0.1:6004"),
            Some(("127.0.0.1".to_string(), 6004))
        );
    }

    #[test]
    fn parse_upstream_missing_port_is_none() {
        assert_eq!(parse_upstream("primary"), None);
    }

    #[test]
    fn parse_upstream_empty_host_is_none() {
        assert_eq!(parse_upstream(":6004"), None);
    }

    #[test]
    fn parse_upstream_ipv6_brackets_kept_in_host() {
        // The bracketed form's brackets stay in the parsed host string
        // (resolve_host strips them later).
        assert_eq!(
            parse_upstream("[::1]:7000"),
            Some(("[::1]".to_string(), 7000))
        );
    }

    #[test]
    fn resolve_host_ipv4_literal() {
        assert_eq!(
            resolve_host("10.0.0.1"),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
        );
    }

    #[test]
    fn resolve_host_ipv6_bracketed_literal_strips() {
        // [::1] is stripped + parsed as IPv6.
        let got = resolve_host("[::1]");
        assert!(matches!(got, Some(IpAddr::V6(_))));
    }
}
