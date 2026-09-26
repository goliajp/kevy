//! Election links under Noise IK: a secure cluster fails over as a plain
//! one does, and a node whose key is not configured is not heard at all.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use kevy_elect::{
    PeerAddr, SecureLinks, Transport,
    elector::{ElectConfig, ElectJitter, Elector},
    message::Role,
};
use kevy_noise::Keypair;
use kevy_testnet::free_ports;

fn fast_cfg() -> ElectConfig {
    ElectConfig {
        hb_interval: Duration::from_millis(50),
        down_after: Duration::from_millis(500),
        election_timeout: Duration::from_millis(500),
        election_backoff: Duration::from_millis(100),
        election_backoff_jitter: Duration::from_millis(0),
    }
}

fn key(n: u8) -> Keypair {
    Keypair::from_secret([n; 32])
}

/// `local` is the key this node actually holds; `keys` is what every node
/// believes each node's public key is.
fn node(
    id: &str,
    port: u16,
    peers: &[(&str, u16)],
    role: Role,
    local: Keypair,
    keys: &[(&str, [u8; 32])],
) -> Transport {
    let ids = std::iter::once(id.to_string())
        .chain(peers.iter().map(|(p, _)| (*p).to_string()))
        .collect();
    let elector = Elector::new(
        id,
        ids,
        format!("127.0.0.1:{port}"),
        role,
        fast_cfg(),
        ElectJitter::Fixed(Duration::ZERO),
    );
    let addrs = peers
        .iter()
        .map(|(p, port)| PeerAddr {
            node_id: (*p).to_string(),
            host: "127.0.0.1".into(),
            port: *port,
        })
        .collect();
    let peer_keys =
        keys.iter().filter(|(p, _)| *p != id).map(|(p, k)| ((*p).to_string(), *k)).collect();
    Transport::spawn_secure(
        elector,
        Duration::from_millis(50),
        (IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        addrs,
        Box::new(|_, _, _| {}),
        SecureLinks { local, peer_keys },
    )
    .expect("spawn transport")
}

fn promoted(nodes: &[(&str, &Transport)], within: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < within {
        for (id, t) in nodes {
            if t.state_snapshot().role == Role::Primary {
                return Some((*id).to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn a_secure_cluster_fails_over() {
    let p = free_ports(3);
    let keys = [("a", key(1).public()), ("b", key(2).public()), ("c", key(3).public())];
    let a = node("a", p[0], &[("b", p[1]), ("c", p[2])], Role::Primary, key(1), &keys);
    let b = node("b", p[1], &[("a", p[0]), ("c", p[2])], Role::Replica, key(2), &keys);
    let c = node("c", p[2], &[("a", p[0]), ("b", p[1])], Role::Replica, key(3), &keys);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        b.state_snapshot().current_primary.as_deref(),
        Some("a"),
        "b heard a over the secure link"
    );
    a.shutdown();
    let winner = promoted(&[("b", &b), ("c", &c)], Duration::from_secs(3));
    b.shutdown();
    c.shutdown();
    assert!(winner.is_some(), "no promotion over secure links");
}

#[test]
fn a_node_with_an_unconfigured_key_is_not_heard() {
    let p = free_ports(3);
    let keys = [("a", key(1).public()), ("b", key(2).public()), ("c", key(3).public())];
    let a = node("a", p[0], &[("b", p[1]), ("c", p[2])], Role::Primary, key(1), &keys);
    let b = node("b", p[1], &[("a", p[0]), ("c", p[2])], Role::Replica, key(2), &keys);
    // c holds a key nobody configured
    let c = node("c", p[2], &[("a", p[0]), ("b", p[1])], Role::Replica, key(9), &keys);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(b.state_snapshot().current_primary.as_deref(), Some("a"));
    assert_eq!(c.state_snapshot().current_primary, None, "c heard someone despite its key");
    assert!(a.state_snapshot().down_peers.contains(&"c".to_string()), "a heard c despite its key");
    assert!(!a.state_snapshot().down_peers.contains(&"b".to_string()), "a hears b");
    a.shutdown();
    // b alone is not a majority of three, and c cannot vote for it
    let winner = promoted(&[("b", &b), ("c", &c)], Duration::from_secs(2));
    b.shutdown();
    c.shutdown();
    assert_eq!(winner, None);
}
