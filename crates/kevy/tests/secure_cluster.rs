//! Cluster mode behind the encrypted client port: every shard's cluster
//! port has an encrypted twin, and a client that came in encrypted is told
//! the encrypted ports while a plaintext one is told the plaintext ports.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kevy_chaos::{Harness, HarnessConfig};
use kevy_resp_client::SecureStream;

const SHARDS: u16 = 4;

struct Node {
    _h: Harness,
    cluster_base: u16,
    secure_cluster_base: u16,
    key: [u8; 32],
    dir: PathBuf,
}

fn wait_listening(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "port {port} never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn start() -> Node {
    // port, cluster ports +1..=+4, encrypted port +6, encrypted cluster +7..=+10
    let port = kevy_testnet::free_port_block(11);
    let (cluster_base, secure, secure_cluster_base) = (port + 1, port + 6, port + 7);
    let dir = std::env::temp_dir().join(format!("kevy-secure-cluster-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = kevy::secure::keygen(&dir.join("server.key")).unwrap();
    let toml = format!(
        "[cluster]\nenabled = true\n[secure]\nprivate_key_file = \"{}\"\nlisten_port = {secure}\n",
        dir.join("server.key").display()
    );
    let cfg = HarnessConfig {
        kevy_bin: PathBuf::from(env!("CARGO_BIN_EXE_kevy")),
        threads: SHARDS as usize,
        ..HarnessConfig::new(dir.join("data"), port).with_extra_toml(toml)
    };
    let h = Harness::spawn(cfg).expect("spawn kevy");
    for p in (0..SHARDS).map(|i| secure_cluster_base + i) {
        wait_listening(p);
    }
    Node { _h: h, cluster_base, secure_cluster_base, key, dir }
}

/// One request, and its whole reply once `done` says it is complete.
fn ask(s: &mut impl ReadWrite, req: &[u8], done: impl Fn(&[u8]) -> bool) -> String {
    s.write_all(req).unwrap();
    let (mut got, mut chunk) = (Vec::new(), [0u8; 4096]);
    while !done(&got) {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "closed after {:?}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

const SLOTS: &[u8] = b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nSLOTS\r\n";

/// A CLUSTER SLOTS reply is complete once it names every shard's port.
fn slots_done(ports: impl Fn(u16) -> u16 + Copy) -> impl Fn(&[u8]) -> bool {
    move |b: &[u8]| {
        let t = String::from_utf8_lossy(b);
        (0..SHARDS).all(|i| t.contains(&format!(":{}\r\n", ports(i))))
    }
}

#[test]
fn encrypted_clients_are_sent_to_encrypted_cluster_ports() {
    let n = start();
    let mut s = SecureStream::connect("127.0.0.1", n.secure_cluster_base, n.key, None).unwrap();
    s.socket().set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let sb = n.secure_cluster_base;
    let slots = ask(&mut s, SLOTS, slots_done(move |i| sb + i));
    for i in 0..SHARDS {
        assert!(!slots.contains(&format!(":{}\r\n", n.cluster_base + i)), "{slots}");
    }
    // a key another shard owns is redirected to that shard's encrypted port
    let get = |key: &str| format!("*2\r\n$3\r\nGET\r\n${}\r\n{key}\r\n", key.len());
    let (key, moved) = (0..200)
        .map(|k| format!("k{k}"))
        .find_map(|key| {
            let r = ask(&mut s, get(&key).as_bytes(), |b| b.ends_with(b"\r\n"));
            r.starts_with("-MOVED").then_some((key, r))
        })
        .expect("some key lives on another shard");
    let port: u16 = moved.trim_end().rsplit(':').next().unwrap().parse().unwrap();
    assert!((sb..sb + SHARDS).contains(&port), "{moved}");
    // and that twin serves the key instead of redirecting again
    let mut t = SecureStream::connect("127.0.0.1", port, n.key, None).unwrap();
    t.socket().set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let served = ask(&mut t, get(&key).as_bytes(), |b| b.ends_with(b"\r\n"));
    assert_eq!(served, "$-1\r\n", "{key} on {port}");
    let _ = std::fs::remove_dir_all(&n.dir);
}

#[test]
fn plaintext_cluster_clients_still_see_plaintext_ports() {
    let n = start();
    let mut p = std::net::TcpStream::connect(("127.0.0.1", n.cluster_base)).unwrap();
    p.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let cb = n.cluster_base;
    let slots = ask(&mut p, SLOTS, slots_done(move |i| cb + i));
    for i in 0..SHARDS {
        assert!(!slots.contains(&format!(":{}\r\n", n.secure_cluster_base + i)), "{slots}");
    }
    let _ = std::fs::remove_dir_all(&n.dir);
}

#[test]
fn a_cluster_client_routes_every_shard_through_encrypted_ports() {
    let n = start();
    let hex: String = n.key.iter().map(|b| format!("{b:02x}")).collect();
    let url = format!("kevys://127.0.0.1:{}?server_key={hex}", n.secure_cluster_base);
    let mut c = kevy_client::ClusterClient::connect_url(&url).unwrap();
    assert_eq!(c.shard_count(), SHARDS as usize);
    for k in 0..100 {
        c.set(format!("ck{k}").as_bytes(), format!("v{k}").as_bytes()).unwrap();
    }
    // a plaintext cluster client sees the same data
    let mut plain =
        kevy_client::ClusterClient::connect_url(&format!("kevy://127.0.0.1:{}", n.cluster_base))
            .unwrap();
    for k in 0..100 {
        let want = format!("v{k}").into_bytes();
        assert_eq!(c.get(format!("ck{k}").as_bytes()).unwrap(), Some(want.clone()));
        assert_eq!(plain.get(format!("ck{k}").as_bytes()).unwrap(), Some(want));
    }
    let _ = std::fs::remove_dir_all(&n.dir);
}
