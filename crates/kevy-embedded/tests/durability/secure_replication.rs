//! Encrypted replication between an embed writer and embed replicas:
//! data flows only between matching keys, and what crosses the wire
//! carries none of it in the clear.

#![cfg(not(target_arch = "wasm32"))]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kevy_embedded::{Config, KevyError, Keypair, LinkKeys, Store};
use kevy_replicate::replica::ReplicaClient;

const MARKER: &[u8] = b"marker-value-7f3a91";

fn key(seed: u8) -> Keypair {
    Keypair::from_secret([seed; 32])
}

fn open_writer(keys: Option<LinkKeys>) -> (Store, String) {
    let mut cfg = Config::default().with_embed_writer("127.0.0.1:0");
    if let Some(k) = keys {
        cfg = cfg.with_writer_security(k);
    }
    let store = Store::open(cfg).unwrap();
    let addr = store.writer_addr().unwrap().to_string();
    (store, addr)
}

fn open_replica(upstream: &str, id: &str, keys: Option<LinkKeys>) -> Store {
    let mut cfg = Config::default()
        .without_aof()
        .with_replica_id(id)
        .with_replica_upstream(upstream)
        .with_replica_reconnect(Duration::from_millis(50), Duration::from_millis(200));
    if let Some(k) = keys {
        cfg = cfg.with_replica_security(k);
    }
    Store::open(cfg).unwrap()
}

fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn arrives(replica: &Store, k: &[u8]) -> bool {
    wait_for(Duration::from_secs(5), || replica.get(k).unwrap().as_deref() == Some(MARKER))
}

fn stays_away(replica: &Store, k: &[u8]) -> bool {
    !wait_for(Duration::from_millis(1500), || replica.get(k).unwrap().is_some())
}

/// A forwarder in front of `target` that records every byte it carries
/// in either direction.
fn recording_proxy(target: String) -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { return };
            let Ok(upstream) = TcpStream::connect(&target) else { continue };
            pipe(client.try_clone().unwrap(), upstream.try_clone().unwrap(), &seen_c);
            pipe(upstream, client, &seen_c);
        }
    });
    (addr, seen)
}

fn pipe(mut from: TcpStream, mut to: TcpStream, seen: &Arc<Mutex<Vec<u8>>>) {
    let seen = Arc::clone(seen);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 || to.write_all(&buf[..n]).is_err() {
                break;
            }
            seen.lock().unwrap().extend_from_slice(&buf[..n]);
        }
        let _ = to.shutdown(Shutdown::Write);
    });
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn replicate_through_proxy(secure: bool) -> Vec<u8> {
    let (w, r) = (key(1), key(2));
    let (writer, addr) =
        open_writer(secure.then(|| LinkKeys { local: w.clone(), peers: vec![r.public()] }));
    writer.set(b"before", MARKER).unwrap();
    let (via, seen) = recording_proxy(addr);
    let replica = open_replica(
        &via,
        if secure { "proxy-secure" } else { "proxy-plain" },
        secure.then(|| LinkKeys { local: r, peers: vec![w.public()] }),
    );
    assert!(arrives(&replica, b"before"), "snapshot did not reach the replica");
    writer.set(b"after", MARKER).unwrap();
    assert!(arrives(&replica, b"after"), "live frame did not reach the replica");
    drop(replica);
    drop(writer);
    let bytes = seen.lock().unwrap().clone();
    assert!(bytes.len() > 64, "the proxy carried only {} bytes", bytes.len());
    bytes
}

#[test]
fn plaintext_link_shows_the_value_on_the_wire() {
    // the control: without it, a proxy that recorded nothing would pass
    // the secure test below
    assert!(contains(&replicate_through_proxy(false), MARKER));
}

#[test]
fn secure_link_carries_the_value_but_never_in_the_clear() {
    let bytes = replicate_through_proxy(true);
    assert!(!contains(&bytes, MARKER));
    assert!(!contains(&bytes, b"REPLICATE"));
}

#[test]
fn replica_tries_each_trusted_key_until_one_answers() {
    let w = key(1);
    let (writer, addr) = open_writer(Some(LinkKeys { local: w.clone(), peers: vec![] }));
    let replica = open_replica(
        &addr,
        "in-turn",
        Some(LinkKeys { local: key(2), peers: vec![key(9).public(), w.public()] }),
    );
    writer.set(b"k", MARKER).unwrap();
    assert!(arrives(&replica, b"k"));
}

#[test]
fn replica_expecting_another_primary_key_gets_nothing() {
    let (writer, addr) = open_writer(Some(LinkKeys { local: key(1), peers: vec![] }));
    let replica = open_replica(
        &addr,
        "wrong-primary",
        Some(LinkKeys { local: key(2), peers: vec![key(9).public()] }),
    );
    writer.set(b"k", MARKER).unwrap();
    assert!(stays_away(&replica, b"k"));
}

#[test]
fn writer_refuses_a_replica_it_does_not_list() {
    let w = key(1);
    let (writer, addr) =
        open_writer(Some(LinkKeys { local: w.clone(), peers: vec![key(8).public()] }));
    let replica =
        open_replica(&addr, "unlisted", Some(LinkKeys { local: key(2), peers: vec![w.public()] }));
    writer.set(b"k", MARKER).unwrap();
    assert!(stays_away(&replica, b"k"));
}

#[test]
fn plaintext_replica_cannot_subscribe_to_a_secure_writer() {
    let (writer, addr) = open_writer(Some(LinkKeys { local: key(1), peers: vec![] }));
    writer.set(b"k", MARKER).unwrap();
    let r = ReplicaClient::connect_with_timeout(addr.as_str(), "plain", 0, Duration::from_secs(1));
    assert!(r.is_err());
}

#[test]
fn secure_replica_without_a_trusted_key_refuses_to_open() {
    let cfg = Config::default()
        .without_aof()
        .with_replica_upstream("127.0.0.1:1")
        .with_replica_security(LinkKeys { local: key(2), peers: vec![] });
    assert!(matches!(Store::open(cfg), Err(KevyError::InvalidInput(_))));
}
