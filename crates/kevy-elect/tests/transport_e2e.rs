//! T1.5.6 TCP transport — real-socket e2e on loopback. Spins up 3
//! `Transport` instances, lets them heartbeat each other, kills the
//! primary, and verifies a replica promotes within the spec'd
//! window.

use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::time::{Duration, Instant};

use kevy_elect::{
    PeerAddr, Transport,
    elector::{ElectConfig, ElectJitter, Elector},
    message::Role,
};

/// Probe 3 free localhost ports. Same pattern as the
/// `kevy/tests/replication.rs` `free_port_block` helper.
fn free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    let ports: Vec<u16> = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    drop(listeners);
    ports
}

fn fast_cfg() -> ElectConfig {
    ElectConfig {
        hb_interval: Duration::from_millis(50),
        down_after: Duration::from_millis(500),
        election_timeout: Duration::from_millis(500),
        election_backoff: Duration::from_millis(100),
        election_backoff_jitter: Duration::from_millis(0),
    }
}

fn build_node(
    node_id: &str,
    listen_port: u16,
    peers: &[(&str, u16)],
    start_role: Role,
) -> Transport {
    let peer_ids: Vec<String> = std::iter::once(node_id.to_string())
        .chain(peers.iter().map(|(id, _)| (*id).to_string()))
        .collect();
    let elector = Elector::new(
        node_id,
        peer_ids,
        format!("127.0.0.1:{listen_port}"),
        start_role,
        fast_cfg(),
        ElectJitter::Fixed(Duration::from_millis(0)),
    );
    let peer_addrs: Vec<PeerAddr> = peers
        .iter()
        .map(|(id, port)| PeerAddr {
            node_id: (*id).to_string(),
            host: "127.0.0.1".to_string(),
            port: *port,
        })
        .collect();
    Transport::spawn(
        elector,
        Duration::from_millis(50),
        (IpAddr::V4(Ipv4Addr::LOCALHOST), listen_port),
        peer_addrs,
    )
    .expect("spawn transport")
}

#[test]
fn three_node_primary_kill_promotes_replica_via_tcp() {
    let ports = free_ports(3);
    let (pa, pb, pc) = (ports[0], ports[1], ports[2]);
    let a = build_node("a", pa, &[("b", pb), ("c", pc)], Role::Primary);
    let b = build_node("b", pb, &[("a", pa), ("c", pc)], Role::Replica);
    let c = build_node("c", pc, &[("a", pa), ("b", pb)], Role::Replica);

    // Let HBs flow so b + c learn that a is primary.
    std::thread::sleep(Duration::from_millis(400));

    let sa = a.state_snapshot();
    let sb = b.state_snapshot();
    let sc = c.state_snapshot();
    assert_eq!(sa.role, Role::Primary, "a started Primary");
    assert_eq!(sb.role, Role::Replica, "b is Replica");
    assert_eq!(sc.role, Role::Replica, "c is Replica");

    a.shutdown();

    let start = Instant::now();
    let mut new_primary: Option<String> = None;
    while start.elapsed() < Duration::from_secs(3) {
        let sb = b.state_snapshot();
        let sc = c.state_snapshot();
        if sb.role == Role::Primary {
            new_primary = Some("b".to_string());
            break;
        }
        if sc.role == Role::Primary {
            new_primary = Some("c".to_string());
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    b.shutdown();
    c.shutdown();
    assert!(
        new_primary.is_some(),
        "no replica promoted within window via TCP",
    );
}
