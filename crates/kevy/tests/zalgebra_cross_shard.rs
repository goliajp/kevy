//! Zset/set algebra `*STORE` family — end-to-end against a real
//! 8-shard reactor, so source keys and `dst` land on different shards
//! and the gather → combine → store two-hop orchestrator
//! (`kevy_rt::exec_zalgebra`) actually crosses cores. Single-store
//! semantics are covered by `kevy-store` unit tests; these prove the
//! wire + routing layer.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

const NSHARDS: usize = 8;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-zalg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS)).bind([127, 0, 0, 1], port).shards(NSHARDS)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Round-trip one command, reading until a full simple reply arrives.
/// Only used for short replies (integers, errors, small arrays).
fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

#[test]
fn zinterstore_cross_shard_weights_aggregate() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"ZADD", b"za", b"1", b"x", b"2", b"y"]);
    cmd(&mut c, &[b"ZADD", b"zb", b"3", b"y", b"4", b"z"]);

    assert_eq!(cmd(&mut c, &[b"ZINTERSTORE", b"zd", b"2", b"za", b"zb"]), b":1\r\n");
    let r = cmd(&mut c, &[b"ZSCORE", b"zd", b"y"]);
    assert_eq!(r, b"$1\r\n5\r\n");

    // WEIGHTS + AGGREGATE MAX: y = max(2*10, 3*1) = 20
    assert_eq!(
        cmd(
            &mut c,
            &[b"ZINTERSTORE", b"zd2", b"2", b"za", b"zb", b"WEIGHTS", b"10", b"1", b"AGGREGATE", b"MAX"]
        ),
        b":1\r\n"
    );
    assert_eq!(cmd(&mut c, &[b"ZSCORE", b"zd2", b"y"]), b"$2\r\n20\r\n");
}

#[test]
fn zunionstore_with_plain_set_and_overwrite() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"ZADD", b"zu-a", b"2", b"y"]);
    cmd(&mut c, &[b"SADD", b"zu-set", b"y", b"q"]);

    // set participates at score 1.0: y = 2 + 1, q = 1
    assert_eq!(cmd(&mut c, &[b"ZUNIONSTORE", b"zu-d", b"2", b"zu-a", b"zu-set"]), b":2\r\n");
    assert_eq!(cmd(&mut c, &[b"ZSCORE", b"zu-d", b"y"]), b"$1\r\n3\r\n");
    assert_eq!(cmd(&mut c, &[b"ZSCORE", b"zu-d", b"q"]), b"$1\r\n1\r\n");

    // *STORE overwrites a dst of ANY type; empty result deletes dst.
    cmd(&mut c, &[b"SET", b"zu-d2", b"oldstring"]);
    assert_eq!(
        cmd(&mut c, &[b"ZINTERSTORE", b"zu-d2", b"2", b"zu-a", b"zu-missing"]),
        b":0\r\n"
    );
    assert_eq!(cmd(&mut c, &[b"EXISTS", b"zu-d2"]), b":0\r\n");
}

#[test]
fn zdiffstore_zintercard_and_errors() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"ZADD", b"dz-a", b"1", b"x", b"2", b"y", b"3", b"z"]);
    cmd(&mut c, &[b"ZADD", b"dz-b", b"9", b"y"]);

    assert_eq!(cmd(&mut c, &[b"ZDIFFSTORE", b"dz-d", b"2", b"dz-a", b"dz-b"]), b":2\r\n");
    assert_eq!(cmd(&mut c, &[b"ZINTERCARD", b"2", b"dz-a", b"dz-b"]), b":1\r\n");
    assert_eq!(
        cmd(&mut c, &[b"ZINTERCARD", b"2", b"dz-a", b"dz-a", b"LIMIT", b"2"]),
        b":2\r\n"
    );

    // WRONGTYPE source aborts the whole op.
    cmd(&mut c, &[b"SET", b"dz-str", b"v"]);
    let r = cmd(&mut c, &[b"ZINTERSTORE", b"dz-d2", b"2", b"dz-a", b"dz-str"]);
    assert!(r.starts_with(b"-WRONGTYPE"), "got {:?}", String::from_utf8_lossy(&r));
    // numkeys = 0 rejected
    let r = cmd(&mut c, &[b"ZINTERCARD", b"0"]);
    assert!(r.starts_with(b"-ERR"), "got {:?}", String::from_utf8_lossy(&r));
}

#[test]
fn set_store_family_cross_shard() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"SADD", b"ss-a", b"a", b"b"]);
    cmd(&mut c, &[b"SADD", b"ss-b", b"b", b"c"]);

    assert_eq!(cmd(&mut c, &[b"SINTERSTORE", b"ss-i", b"ss-a", b"ss-b"]), b":1\r\n");
    assert_eq!(cmd(&mut c, &[b"SISMEMBER", b"ss-i", b"b"]), b":1\r\n");
    assert_eq!(cmd(&mut c, &[b"SUNIONSTORE", b"ss-u", b"ss-a", b"ss-b"]), b":3\r\n");
    assert_eq!(cmd(&mut c, &[b"SDIFFSTORE", b"ss-d", b"ss-a", b"ss-b"]), b":1\r\n");
    assert_eq!(cmd(&mut c, &[b"SISMEMBER", b"ss-d", b"a"]), b":1\r\n");
}

#[test]
fn zalgebra_survives_restart_via_effect_aof() {
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "kevy-zalg-restart-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let boot = |port: u16, stop: Arc<AtomicBool>, dir: std::path::PathBuf| {
        std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS)).bind([127, 0, 0, 1], port).shards(NSHARDS)
                .with_data_dir(dir);
            rt.run(stop).unwrap();
        })
    };
    let wait_up = |port: u16| {
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("server did not come up");
    };

    // Generation 1: write via the orchestrator, stop cleanly (the
    // shutdown path flushes each shard's AOF).
    let port1 = free_port();
    let stop1 = Arc::new(AtomicBool::new(false));
    let h1 = boot(port1, stop1.clone(), dir.clone());
    wait_up(port1);
    {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port1)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        cmd(&mut c, &[b"ZADD", b"ra", b"1", b"x", b"2", b"y"]);
        cmd(&mut c, &[b"ZADD", b"rb", b"3", b"y"]);
        assert_eq!(cmd(&mut c, &[b"ZINTERSTORE", b"rd", b"2", b"ra", b"rb"]), b":1\r\n");
    }
    stop1.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port1));
    h1.join().unwrap();

    // Generation 2: same data dir; the effect frames (DEL + ZADD)
    // must rebuild `rd`.
    let port2 = free_port();
    let stop2 = Arc::new(AtomicBool::new(false));
    let h2 = boot(port2, stop2.clone(), dir.clone());
    wait_up(port2);
    {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port2)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        assert_eq!(cmd(&mut c, &[b"ZSCORE", b"rd", b"y"]), b"$1\r\n5\r\n");
    }
    stop2.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port2));
    h2.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
