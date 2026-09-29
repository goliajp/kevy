//! Replication over Noise IK, in process: the primary's reactor and the
//! replica client both run here, so the encrypted paths on each side are
//! the ones under test.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kevy_noise::Keypair;
use kevy_replicate::replica::{ReplicaClient, ReplicaEvent, ReplicaSecurity};
use kevy_rt::ReplicationSecurity;
use kevy_testnet::free_port_block;
use kevy_tmpdir::TmpDir;

static START_GATE: Mutex<()> = Mutex::new(());

struct Primary {
    port: u16,
    repl: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: TmpDir,
}

impl Primary {
    fn start(security: ReplicationSecurity) -> Primary {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = free_port_block(1);
        let (port, repl) = (base, base + 1);
        let dir = TmpDir::new("kevy-secure-replication");
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        // SAFETY: the replication listener runs on the epoll/kqueue reactor;
        // no other thread reads this variable concurrently in this process.
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
                .bind([127, 0, 0, 1], port)
                .shards(1)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replication(true, 1024 * 1024)
                .with_replication_listener(repl)
                .with_replication_security(security);
            let _ = rt.run(stop_thread);
        });
        for p in [port, repl] {
            let mut up = false;
            for _ in 0..2000 {
                if std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() {
                    up = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(up, "port {p} never came up");
        }
        Primary { port, repl, stop, handle: Some(handle), _dir: dir }
    }

    fn info_replication(&self) -> String {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        c.write_all(b"*2\r\n$4\r\nINFO\r\n$11\r\nreplication\r\n").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        // a bulk reply: header line, then the body; read until the body ends
        loop {
            let n = c.read(&mut chunk).unwrap();
            out.extend_from_slice(&chunk[..n]);
            if n == 0 || out.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn set(&self, key: &str, value: &str) {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let req = format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{value}\r\n",
            key.len(),
            value.len()
        );
        c.write_all(req.as_bytes()).unwrap();
        let mut ok = [0u8; 5];
        c.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"+OK\r\n");
    }
}

impl Drop for Primary {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn keys() -> (Keypair, Keypair) {
    (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]))
}

fn connect(
    p: &Primary,
    local: &Keypair,
    primary_key: [u8; 32],
) -> Result<ReplicaClient, kevy_replicate::replica::ReplicaError> {
    let sec = ReplicaSecurity::new(local.clone(), primary_key);
    ReplicaClient::connect_secure(
        ("127.0.0.1", p.repl),
        "secure-replica",
        0,
        0,
        Duration::from_secs(5),
        &sec,
    )
}

/// Read events until a live frame for `key` arrives.
fn wait_for_set(client: &mut ReplicaClient, key: &[u8]) {
    client.socket_handle().unwrap().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    for _ in 0..10_000 {
        match client.next_event().expect("stream ended").expect("stream error") {
            ReplicaEvent::Frame(f) if f.argv.get(1) == Some(key) => {
                assert_eq!(f.argv.get(0), Some(&b"SET"[..]));
                return;
            }
            _ => {}
        }
    }
    panic!("no frame for the key");
}

#[test]
fn writes_reach_a_replica_holding_the_right_keys() {
    let (primary_key, replica_key) = keys();
    let p = Primary::start(ReplicationSecurity {
        local: primary_key.clone(),
        replica_keys: vec![replica_key.public()],
    });
    p.set("before", "1");
    let mut client = connect(&p, &replica_key, primary_key.public()).expect("secure handshake");
    p.set("after", "2");
    wait_for_set(&mut client, b"after");
    // the ack travels back over the same encrypted link, and the primary
    // reads it: the replica turns online only once an ack is parsed
    client.send_ack(client.expected_offset()).unwrap();
    let mut online = false;
    for _ in 0..200 {
        if p.info_replication().contains("state=online") {
            online = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(online, "the primary never read the encrypted ack: {}", p.info_replication());
    // a large value spans several Noise messages
    let big = "v".repeat(200_000);
    p.set("big", &big);
    wait_for_set(&mut client, b"big");
}

#[test]
fn a_replica_expecting_another_primary_key_fails_the_handshake() {
    let (primary_key, replica_key) = keys();
    let p = Primary::start(ReplicationSecurity { local: primary_key, replica_keys: Vec::new() });
    let wrong = Keypair::from_secret([7; 32]).public();
    assert!(connect(&p, &replica_key, wrong).is_err());
}

#[test]
fn a_replica_outside_replica_keys_is_refused() {
    let (primary_key, replica_key) = keys();
    let p = Primary::start(ReplicationSecurity {
        local: primary_key.clone(),
        replica_keys: vec![replica_key.public()],
    });
    let stranger = Keypair::from_secret([8; 32]);
    assert!(connect(&p, &stranger, primary_key.public()).is_err());
    // the listed replica still gets in afterwards
    assert!(connect(&p, &replica_key, primary_key.public()).is_ok());
}

#[test]
fn an_empty_replica_list_admits_any_replica_encrypted() {
    let (primary_key, _) = keys();
    let p = Primary::start(ReplicationSecurity {
        local: primary_key.clone(),
        replica_keys: Vec::new(),
    });
    let anyone = Keypair::from_secret([9; 32]);
    let mut client =
        connect(&p, &anyone, primary_key.public()).expect("open primary admits any key");
    p.set("k", "v");
    wait_for_set(&mut client, b"k");
}

#[test]
fn a_plaintext_replica_gets_nothing_from_a_secure_primary() {
    let (primary_key, _) = keys();
    let p = Primary::start(ReplicationSecurity { local: primary_key, replica_keys: Vec::new() });
    p.set("secret", "value");
    let plain =
        ReplicaClient::connect_at(("127.0.0.1", p.repl), "plain", 0, 0, Duration::from_secs(2));
    assert!(plain.is_err(), "a plaintext handshake must not be answered");
}
