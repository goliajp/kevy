//! The encrypted client port on a real server: commands work over
//! `kevys://`, the server sees the real client address, admission follows
//! `client_keys`, and the plaintext port is untouched.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};
use kevy_resp_client::{Keypair, Reply, RespClient, SecureStream, load_client_key};

struct Server {
    _h: Harness,
    port: u16,
    secure_port: u16,
    server_key: [u8; 32],
    client_key_file: PathBuf,
    dir: PathBuf,
}

fn hex(k: &[u8; 32]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

fn start() -> Server {
    let (port, secure_port) = (pick_free_port().unwrap(), pick_free_port().unwrap());
    let dir = std::env::temp_dir().join(format!("kevy-secure-clients-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let server_key = kevy::secure::keygen(&dir.join("server.key")).unwrap();
    let client_key_file = dir.join("client.key");
    let client_pub = kevy::secure::keygen(&client_key_file).unwrap();
    let toml = format!(
        "[secure]\nprivate_key_file = \"{}\"\nlisten_port = {secure_port}\nclient_keys = [\"{}\"]\n",
        dir.join("server.key").display(),
        hex(&client_pub)
    );
    let cfg = HarnessConfig {
        kevy_bin: PathBuf::from(env!("CARGO_BIN_EXE_kevy")),
        threads: 2,
        ..HarnessConfig::new(dir.join("data"), port).with_extra_toml(toml)
    };
    let h = Harness::spawn(cfg).expect("spawn kevy");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", secure_port)).is_err() {
        assert!(Instant::now() < deadline, "the encrypted port never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
    Server { _h: h, port, secure_port, server_key, client_key_file, dir }
}

fn bulk(r: Reply) -> Vec<u8> {
    match r {
        Reply::Bulk(b) => b,
        other => panic!("expected a bulk reply, got {other:?}"),
    }
}

#[test]
fn the_encrypted_port_serves_listed_clients_and_names_them() {
    let s = start();
    let url = format!(
        "kevys://127.0.0.1:{}?server_key={}&client_key_file={}",
        s.secure_port,
        hex(&s.server_key),
        s.client_key_file.display()
    );
    let mut c = RespClient::connect_url(&url).unwrap();
    let big = vec![b'v'; 300_000];
    assert_eq!(c.request_borrowed(&[b"SET", b"k", &big]).unwrap(), Reply::Simple(b"OK".to_vec()));
    assert_eq!(bulk(c.request_borrowed(&[b"GET", b"k"]).unwrap()), big);

    // the plaintext port is unchanged and sees the same data
    let mut plain = RespClient::connect("127.0.0.1", s.port).unwrap();
    assert_eq!(bulk(plain.request_borrowed(&[b"GET", b"k"]).unwrap()).len(), big.len());

    // CLIENT INFO names this client's own address, not the relay's
    let me = load_client_key(&s.client_key_file).unwrap();
    let mut raw =
        SecureStream::connect("127.0.0.1", s.secure_port, s.server_key, Some(&me)).unwrap();
    let mine = raw.socket().local_addr().unwrap().to_string();
    raw.write_all(b"*2\r\n$6\r\nCLIENT\r\n$4\r\nINFO\r\n").unwrap();
    raw.socket().set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut got = Vec::new();
    let mut chunk = [0u8; 512];
    while !got.ends_with(b"\r\n") || got.len() < 8 {
        let n = raw.read(&mut chunk).unwrap();
        assert!(n > 0, "closed after {got:?}");
        got.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.contains(&format!("addr={mine} ")), "{text} should name {mine}");
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[test]
fn unlisted_clients_and_a_wrong_server_key_get_no_session() {
    let s = start();
    let listed = load_client_key(&s.client_key_file).unwrap();
    let stranger = Keypair::from_secret([7; 32]);
    let host = "127.0.0.1";
    assert!(SecureStream::connect(host, s.secure_port, s.server_key, Some(&stranger)).is_err());
    assert!(SecureStream::connect(host, s.secure_port, s.server_key, None).is_err());
    assert!(SecureStream::connect(host, s.secure_port, [9; 32], Some(&listed)).is_err());
    assert!(RespClient::connect_secure(host, s.secure_port, s.server_key, Some(&listed)).is_ok());
    // a plaintext client on the encrypted port gets no reply
    let mut p = std::net::TcpStream::connect((host, s.secure_port)).unwrap();
    p.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    p.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut buf = [0u8; 8];
    assert!(matches!(p.read(&mut buf), Ok(0) | Err(_)));
    let _ = std::fs::remove_dir_all(&s.dir);
}
