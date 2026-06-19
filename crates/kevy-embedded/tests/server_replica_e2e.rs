//! Phase 2 / T2.8, T2.10, T2.11 — end-to-end: real kevy server primary
//! in-process + `kevy_embedded::Store::open_replica` against its
//! replication port. Verifies the full chain — handshake, streaming,
//! apply, READONLY enforcement — over a real socket, the same path
//! production traffic takes.

#![cfg(not(target_arch = "wasm32"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};

/// Serialise `Server::start` calls: each test's reactor binds a fresh
/// pair of ports, and the bind-bind race is real when tests run in
/// parallel (the second `free_port_block` can reserve the port the
/// first `Runtime` is still mid-binding).
static START_GATE: Mutex<()> = Mutex::new(());

/// Stand-in for the `tempfile` crate (workspace 0-dep rule).
mod tempdir {
    use std::path::PathBuf;
    pub struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        pub fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("{label}-{nanos}"));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
        pub fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Reserve a contiguous block of `width` free ports starting at some
/// random base — same trick as `crates/kevy/tests/replication.rs`
/// `free_port_block`, kept private here to avoid pulling that file
/// across crate boundaries.
fn free_port_block(width: usize) -> u16 {
    'retry: loop {
        let anchor = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = anchor.local_addr().unwrap().port();
        if base.checked_add(width as u16).is_none() {
            continue;
        }
        let mut probes = Vec::with_capacity(width);
        for i in 1..=width as u16 {
            match TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => probes.push(l),
                Err(_) => continue 'retry,
            }
        }
        return base;
    }
}

struct Server {
    port: u16,
    replication_base: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempdir::TempDir,
}

impl Server {
    fn start() -> Server {
        let _gate = START_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: integration test owns its own process state; setting
        // an env var here is safe since no other thread reads
        // KEVY_IO_URING in parallel. Replication listener is gated to
        // epoll/kqueue in v1.18/19.
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }
        let base = free_port_block(2);
        let port = base;
        let replication_base = base + 1;
        let dir = tempdir::TempDir::new("kevy-embed-replica-e2e");
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replication(true, 1024 * 1024)
                .with_replication_listener(replication_base);
            let _ = rt.run(stop_t);
        });
        // Wait for both ports.
        for p in [port, replication_base] {
            let mut ready = false;
            for _ in 0..400 {
                if std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() {
                    ready = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(ready, "server did not come up on port {p}");
        }
        Server {
            port,
            replication_base,
            stop,
            handle: Some(handle),
            _dir: dir,
        }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Send one RESP command over the compat port and read enough of
    /// the reply to confirm it. We only care that the write applied,
    /// so anything that starts with `+OK` / `$` / `:` is success.
    fn cmd(&self, parts: &[&[u8]]) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut req: Vec<u8> = Vec::new();
        req.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
        for p in parts {
            req.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            req.extend_from_slice(p);
            req.extend_from_slice(b"\r\n");
        }
        s.write_all(&req).unwrap();
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).unwrap_or(0);
        assert!(n > 0, "no reply to {parts:?}");
        let head = buf[0];
        assert!(
            head == b'+' || head == b'$' || head == b':',
            "unexpected reply head {head:?} for {parts:?}: {:?}",
            String::from_utf8_lossy(&buf[..n]),
        );
    }
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn server_primary_streams_to_embed_replica() {
    let server = Server::start();
    let upstream = format!("127.0.0.1:{}", server.replication_base);
    let replica = Store::open_replica(&upstream).unwrap();
    assert!(replica.is_replica(), "open_replica did not set replica flag");

    // Write to the server primary; verify the embed replica sees it.
    server.cmd(&[b"SET", b"key-a", b"hello"]);
    server.cmd(&[b"SET", b"key-b", b"world"]);

    let saw_both = wait_for(Duration::from_secs(5), || {
        replica.get(b"key-a").unwrap().as_deref() == Some(b"hello".as_slice())
            && replica.get(b"key-b").unwrap().as_deref() == Some(b"world".as_slice())
    });
    assert!(
        saw_both,
        "embed replica never observed both SET writes within timeout"
    );

    drop(replica);
    server.shutdown();
}

#[test]
fn embed_replica_rejects_local_writes_with_readonly() {
    // No real server needed — `open_replica` configures READONLY based
    // on `Config::replica_upstream`. We pass a bogus upstream; the
    // runner thread will spin in reconnect, but the public API gate
    // doesn't depend on the runner having connected.
    let cfg = Config::default()
        .with_replica_upstream("127.0.0.1:1") // port 1 = always refused
        .with_replica_reconnect(Duration::from_millis(50), Duration::from_millis(100));
    let replica = Store::open(cfg).unwrap();

    let err = replica.set(b"k", b"v").expect_err("write should be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("READONLY"),
        "expected READONLY error, got: {msg}"
    );

    // Reads still work.
    assert_eq!(replica.get(b"k").unwrap(), None);
    // Other mutators also refused.
    assert!(replica.del(&[&b"k"[..]]).is_err());
    assert!(replica.incr_by(b"n", 1).is_err());
    assert!(replica.hset(b"h", &[(&b"f"[..], &b"v"[..])]).is_err());
    assert!(replica.lpush(b"l", &[&b"x"[..]]).is_err());
    assert!(replica.sadd(b"s", &[&b"m"[..]]).is_err());
    assert!(replica.zadd(b"z", &[(1.0, &b"m"[..])]).is_err());
}

#[test]
fn embed_replica_streams_multiple_writes_on_same_connection() {
    // T2.8 follow-up: validate that a single ReplicaClient session
    // ingests an arbitrary sequence of writes without re-handshaking.
    // (The streaming-pump in `kevy_rt::replication_pump` already
    // handles this for server↔server; this asserts the embed runner
    // path doesn't break it.)
    let server = Server::start();
    let upstream = format!("127.0.0.1:{}", server.replication_base);
    let cfg = Config::default()
        .with_replica_upstream(&upstream)
        .with_replica_reconnect(Duration::from_millis(30), Duration::from_millis(100));
    let replica = Store::open(cfg).unwrap();

    server.cmd(&[b"SET", b"k1", b"v1"]);
    assert!(
        wait_for(Duration::from_secs(5), || {
            replica.get(b"k1").unwrap().as_deref() == Some(b"v1".as_slice())
        }),
        "embed replica missed first write"
    );
    server.cmd(&[b"SET", b"k2", b"v2"]);
    assert!(
        wait_for(Duration::from_secs(5), || {
            replica.get(b"k2").unwrap().as_deref() == Some(b"v2".as_slice())
        }),
        "embed replica missed second write on the same connection"
    );

    drop(replica);
    server.shutdown();
}

#[test]
fn fresh_embed_catches_up_against_existing_primary_backlog() {
    // T2.11: an embed that opens AFTER the primary has already
    // applied writes catches up by streaming from offset 0 against
    // the primary's backlog. This is the v1.20 MVP path — the
    // primary's backlog must still cover offset 0 (snapshot ingest
    // is not yet wired; if the backlog has rolled past offset 0 the
    // primary will need a future snapshot-ship handshake).
    let server = Server::start();
    // Pre-fill the primary; embed has not connected yet.
    for i in 0..10 {
        let k = format!("pre-{i}");
        let v = format!("v-{i}");
        server.cmd(&[b"SET", k.as_bytes(), v.as_bytes()]);
    }
    // Now open embed; runner handshakes at offset 0 and streams
    // the existing backlog from there.
    let upstream = format!("127.0.0.1:{}", server.replication_base);
    let replica = Store::open_replica(&upstream).unwrap();
    assert!(
        wait_for(Duration::from_secs(5), || {
            (0..10).all(|i| {
                let k = format!("pre-{i}");
                let v = format!("v-{i}");
                replica.get(k.as_bytes()).unwrap().as_deref() == Some(v.as_bytes())
            })
        }),
        "fresh embed never caught up with existing backlog"
    );
    // Live writes after embed is caught up also flow through.
    server.cmd(&[b"SET", b"post-write", b"post-val"]);
    assert!(
        wait_for(Duration::from_secs(5), || {
            replica.get(b"post-write").unwrap().as_deref() == Some(b"post-val".as_slice())
        }),
        "embed missed a write that arrived after catch-up"
    );
    drop(replica);
    server.shutdown();
}

#[test]
fn embed_retargets_to_new_primary_via_set_replica_upstream() {
    // T2.7 + T2.9: an application observing a kevy-elect ANNOUNCE
    // (or any other failover signal) calls `Store::set_replica_upstream`
    // to point the embed runner at the new primary. The runner
    // shuts the live socket so the retarget lands within
    // `replica_reconnect_min` — verified here against a real
    // second server.
    let server_a = Server::start();
    let upstream_a = format!("127.0.0.1:{}", server_a.replication_base);
    let replica = Store::open_replica(&upstream_a).unwrap();
    server_a.cmd(&[b"SET", b"from-a", b"a-val"]);
    assert!(
        wait_for(Duration::from_secs(5), || {
            replica.get(b"from-a").unwrap().as_deref() == Some(b"a-val".as_slice())
        }),
        "embed never observed write from primary A"
    );

    // Bring up primary B (independent server, fresh data dir).
    let server_b = Server::start();
    let upstream_b = format!("127.0.0.1:{}", server_b.replication_base);
    server_b.cmd(&[b"SET", b"from-b", b"b-val"]);

    // Application-level retarget: tells embed "primary B is the new
    // upstream". The runner drops the A connection + reconnects to B
    // within `replica_reconnect_min`.
    replica
        .set_replica_upstream(&upstream_b)
        .expect("retarget should succeed on a replica store");

    assert!(
        wait_for(Duration::from_secs(5), || {
            replica.get(b"from-b").unwrap().as_deref() == Some(b"b-val".as_slice())
        }),
        "embed never observed write from primary B after retarget"
    );

    drop(replica);
    server_a.shutdown();
    server_b.shutdown();
}

#[test]
fn set_replica_upstream_on_non_replica_returns_error() {
    let s = Store::open(Config::default()).unwrap();
    assert!(!s.is_replica());
    let err = s
        .set_replica_upstream("127.0.0.1:1")
        .expect_err("should error on a non-replica");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn embed_restart_resumes_via_fresh_handshake() {
    // T2.11: dropping the embed store stops its runner thread; opening
    // a new embed against the same primary re-handshakes from offset
    // 0 and re-applies the full backlog. The new embed has no AOF
    // (replicas force-disable it) so its state is necessarily empty
    // at open and catch-up replays from scratch — exactly the
    // semantics promised in the v1.20 anti-scope (no snapshot ingest,
    // no offset-persistence on the replica side).
    let server = Server::start();
    let upstream = format!("127.0.0.1:{}", server.replication_base);

    {
        let r1 = Store::open_replica(&upstream).unwrap();
        server.cmd(&[b"SET", b"persist", b"a"]);
        assert!(
            wait_for(Duration::from_secs(5), || {
                r1.get(b"persist").unwrap().as_deref() == Some(b"a".as_slice())
            }),
            "first embed never observed initial write"
        );
        drop(r1);
    }
    // Give the server time to (a) sweep r1's closed conn into a slot
    // entry, (b) run the per-shard tick that evicts frame 0 (since the
    // slot now reports sent_offset=1). After this the primary's
    // backlog has rolled past offset 0; r2 connecting at from_offset=0
    // exercises the snapshot-ship path on the server, ingested by the
    // embed runner.
    std::thread::sleep(Duration::from_millis(200));

    // Embed restart: open a fresh replica with a new replica_id (the
    // unique `kevy-embedded-{pid}-{seq}` id is the default). It
    // handshakes from offset 0, the primary detects TooOld and ships
    // a snapshot; the runner ingests it and applies into shard 0.
    let r2 = Store::open_replica(&upstream).unwrap();
    assert!(
        wait_for(Duration::from_secs(15), || {
            r2.get(b"persist").unwrap().as_deref() == Some(b"a".as_slice())
        }),
        "restarted embed never re-applied the existing backlog via snapshot ship"
    );
    drop(r2);
    server.shutdown();
}
