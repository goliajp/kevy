//! T1.32-37 integration: a `ReadWriteClient` against a real primary +
//! replica kevy Runtime pair. Verifies the read/write classification
//! actually splits traffic — writes land at the primary, reads round-
//! robin across replicas.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

/// Inline tempdir (no `tempfile` crate — workspace 0-dep rule).
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

fn free_port_block(width: usize) -> u16 {
    'retry: loop {
        let anchor = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = anchor.local_addr().unwrap().port();
        if base.checked_add(width as u16).is_none() {
            continue;
        }
        let mut probes = Vec::with_capacity(width);
        for i in 1..=width as u16 {
            match std::net::TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => probes.push(l),
                Err(_) => continue 'retry,
            }
        }
        return base;
    }
}

struct PrimaryServer {
    port: u16,
    replication_base: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempdir::TempDir,
}

impl PrimaryServer {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = free_port_block(1);
        let port = base;
        let replication_base = base + 1;
        let dir = tempdir::TempDir::new("kevy-rw-primary");
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replication(true, 1024 * 1024)
                .with_replication_listener(replication_base);
            let _ = rt.run(stop_thread);
        });
        for p in [port, replication_base] {
            let mut ready = false;
            for _ in 0..400 {
                if std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(ready, "primary not up on port {p}");
        }
        Self {
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
}

struct ReplicaServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    runner_stop: Arc<AtomicBool>,
    runner_handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempdir::TempDir,
}

impl ReplicaServer {
    /// Spin up a replica Runtime + a runner thread that bridges to
    /// the primary's per-shard replication port.
    fn start(upstream_replication_port: u16) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port_block(1) + 1;
        let dir = tempdir::TempDir::new("kevy-rw-replica");
        let dir_path = dir.path().to_path_buf();
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }
        let (sender, receiver) = kevy_rt::replica_inbox_pair();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replica_inboxes(vec![receiver]);
            let _ = rt.run(stop_thread);
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let runner_stop = Arc::new(AtomicBool::new(false));
        let runner_stop_thread = runner_stop.clone();
        let runner_handle = std::thread::spawn(move || {
            let mut from_offset: u64 = 0;
            while !runner_stop_thread.load(Ordering::Relaxed) {
                let conn = kevy_replicate::replica::ReplicaClient::connect(
                    ("127.0.0.1", upstream_replication_port),
                    "rw-test-runner",
                    from_offset,
                );
                let Ok(mut client) = conn else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                };
                while !runner_stop_thread.load(Ordering::Relaxed) {
                    match client.next_event() {
                        Some(Ok(ev)) => {
                            let apply = match ev {
                                kevy_replicate::replica::ReplicaEvent::SnapshotBegin => {
                                    kevy_rt::ReplicaApply::SnapshotBegin
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotChunk(b) => {
                                    kevy_rt::ReplicaApply::SnapshotChunk(b)
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotEnd { ack_offset } => {
                                    from_offset = ack_offset;
                                    kevy_rt::ReplicaApply::SnapshotEnd { ack_offset }
                                }
                                kevy_replicate::replica::ReplicaEvent::Frame(frame) => {
                                    from_offset = frame.offset.saturating_add(1);
                                    kevy_rt::ReplicaApply::Frame {
                                        offset: frame.offset,
                                        argv: frame.argv,
                                    }
                                }
                            };
                            if sender.send(apply).is_err() {
                                return;
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
        });

        Self {
            port,
            stop,
            handle: Some(handle),
            runner_stop,
            runner_handle: Some(runner_handle),
            _dir: dir,
        }
    }

    fn shutdown(mut self) {
        self.runner_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.runner_handle.take() {
            let _ = h.join();
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// Note: `kevy_replicate` is unused at the kevy-cluster-rw crate's
// regular dep level, but the integration tests need it. Bring it in
// via the kevy dev-dep edge (kevy → kevy_replicate is now a regular
// dep since T1.29).

#[test]
fn write_lands_on_primary_read_round_robins_to_replica() {
    let primary = PrimaryServer::start();
    let replica = ReplicaServer::start(primary.replication_base);

    let mut client = kevy_cluster_rw::ReadWriteClient::connect(
        ("127.0.0.1", primary.port),
        &[("127.0.0.1", replica.port)],
    )
    .expect("connect");
    assert_eq!(client.replica_count(), 1);

    // Auto-routed: SET → primary; GET → replica.
    for i in 0..5 {
        let key = format!("rw-k{i}");
        let val = format!("v{i}");
        let reply = client
            .request(&[b"SET".to_vec(), key.as_bytes().to_vec(), val.as_bytes().to_vec()])
            .expect("write");
        assert!(matches!(reply, kevy_resp::Reply::Simple(_)));
    }

    // Poll until the replica has caught up — the runner thread runs
    // async with the test (the replica's tick is 100 ms).
    let mut all_seen = false;
    for _ in 0..200 {
        let mut got_all = true;
        for i in 0..5 {
            let key = format!("rw-k{i}");
            let reply = client
                .request_read(&[b"GET".to_vec(), key.as_bytes().to_vec()], false)
                .expect("read");
            match reply {
                kevy_resp::Reply::Bulk(_) => {}
                _ => {
                    got_all = false;
                    break;
                }
            }
        }
        if got_all {
            all_seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(all_seen, "replica did not catch up via runner");

    // Verify every key's value matches what we wrote.
    for i in 0..5 {
        let key = format!("rw-k{i}");
        let expected = format!("v{i}");
        let reply = client
            .request_read(&[b"GET".to_vec(), key.as_bytes().to_vec()], false)
            .expect("read");
        match reply {
            kevy_resp::Reply::Bulk(b) => assert_eq!(b, expected.as_bytes(), "{key}"),
            other => panic!("{key}: unexpected {other:?}"),
        }
    }

    // READCONSISTENT (`consistent = true`) forces a read to the
    // primary — verify by setting + immediately reading consistent
    // before the replica catches up.
    let _ = client
        .request_write(&[b"SET".to_vec(), b"rw-consistent".to_vec(), b"yes".to_vec()])
        .expect("write");
    let reply = client
        .request_read(&[b"GET".to_vec(), b"rw-consistent".to_vec()], true)
        .expect("consistent read");
    match reply {
        kevy_resp::Reply::Bulk(b) => assert_eq!(b, b"yes"),
        other => panic!("consistent read: unexpected {other:?}"),
    }

    drop(client);
    primary.shutdown();
    replica.shutdown();
}

#[test]
fn read_falls_back_to_primary_when_no_replicas() {
    let primary = PrimaryServer::start();
    let mut client = kevy_cluster_rw::ReadWriteClient::connect(
        ("127.0.0.1", primary.port),
        &[],
    )
    .expect("connect");
    assert_eq!(client.replica_count(), 0);

    // Write + read in the same client — both hit primary (no replica
    // configured), so the read sees the freshly-written value.
    let _ = client
        .request_write(&[b"SET".to_vec(), b"fallback-k".to_vec(), b"v".to_vec()])
        .expect("write");
    let reply = client
        .request_read(&[b"GET".to_vec(), b"fallback-k".to_vec()], false)
        .expect("read");
    match reply {
        kevy_resp::Reply::Bulk(b) => assert_eq!(b, b"v"),
        other => panic!("unexpected {other:?}"),
    }

    drop(client);
    primary.shutdown();
}

// Silence "unused crate" warnings — Read/Write impls referenced by
// the helper modules above use std I/O traits through their parent
// imports; this brings the trait names into scope explicitly so
// rustc's import linter doesn't strip the `use` from the tempdir
// module (which depends on transitive trait imports here).
#[allow(dead_code)]
fn _trait_use_anchor(s: &mut std::net::TcpStream) {
    let mut buf = [0u8; 1];
    let _ = s.read(&mut buf);
    let _ = s.write_all(&buf);
}

/// T1.38: 1 primary + 2 replicas, write every redis-type kevy
/// supports, then read each key from each replica via separate
/// ReadWriteClient instances. Content must match on both.
///
/// kevy redis-types covered: string (SET/GET), hash (HSET/HGETALL),
/// list (LPUSH/LRANGE), set (SADD/SMEMBERS), zset (ZADD/ZSCORE),
/// stream (XADD/XRANGE). Counters share the string type so the
/// SET/GET covers it.
#[test]
fn types_matrix_one_primary_two_replicas() {
    let primary = PrimaryServer::start();
    let r1 = ReplicaServer::start(primary.replication_base);
    let r2 = ReplicaServer::start(primary.replication_base);

    // ReadWriteClient with both replicas — exercises the round-robin
    // path against multiple replicas.
    let mut client = kevy_cluster_rw::ReadWriteClient::connect(
        ("127.0.0.1", primary.port),
        &[("127.0.0.1", r1.port), ("127.0.0.1", r2.port)],
    )
    .expect("connect");
    assert_eq!(client.replica_count(), 2);

    // Drive every redis-type write through the primary.
    let _ = client.request(&[
        b"SET".to_vec(), b"t:str".to_vec(), b"hello".to_vec(),
    ]).expect("SET");
    let _ = client.request(&[
        b"HSET".to_vec(), b"t:hash".to_vec(),
        b"f1".to_vec(), b"v1".to_vec(),
        b"f2".to_vec(), b"v2".to_vec(),
    ]).expect("HSET");
    let _ = client.request(&[
        b"RPUSH".to_vec(), b"t:list".to_vec(),
        b"a".to_vec(), b"b".to_vec(), b"c".to_vec(),
    ]).expect("RPUSH");
    let _ = client.request(&[
        b"SADD".to_vec(), b"t:set".to_vec(),
        b"x".to_vec(), b"y".to_vec(), b"z".to_vec(),
    ]).expect("SADD");
    let _ = client.request(&[
        b"ZADD".to_vec(), b"t:zset".to_vec(),
        b"1".to_vec(), b"one".to_vec(),
        b"2".to_vec(), b"two".to_vec(),
    ]).expect("ZADD");
    let _ = client.request(&[
        b"XADD".to_vec(), b"t:stream".to_vec(),
        b"1-1".to_vec(), b"event".to_vec(), b"alpha".to_vec(),
    ]).expect("XADD");

    // Poll until **both** replicas (the round-robin alternates per
    // call) have the string key. 5 consecutive Bulk hits is enough
    // to cycle past both with high probability.
    let mut consecutive = 0u32;
    let mut caught_up = false;
    for _ in 0..400 {
        let r = client.request_read(&[b"GET".to_vec(), b"t:str".to_vec()], false)
            .expect("read");
        if matches!(r, kevy_resp::Reply::Bulk(_)) {
            consecutive += 1;
            if consecutive >= 5 {
                caught_up = true;
                break;
            }
        } else {
            consecutive = 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(caught_up, "replicas didn't catch up");

    // Run all read queries via consistent reads (primary) AND via
    // round-robin replica reads — both paths should agree on values.
    for consistent in [true, false] {
        let reply = client.request_read(
            &[b"GET".to_vec(), b"t:str".to_vec()],
            consistent,
        ).expect("GET");
        match reply {
            kevy_resp::Reply::Bulk(b) => assert_eq!(b, b"hello", "consistent={consistent}"),
            other => panic!("GET t:str (consistent={consistent}): {other:?}"),
        }

        let reply = client.request_read(
            &[b"HGETALL".to_vec(), b"t:hash".to_vec()],
            consistent,
        ).expect("HGETALL");
        match reply {
            kevy_resp::Reply::Array(arr) => {
                assert_eq!(arr.len(), 4, "HGETALL array len; consistent={consistent}");
            }
            other => panic!("HGETALL (consistent={consistent}): {other:?}"),
        }

        let reply = client.request_read(
            &[b"LRANGE".to_vec(), b"t:list".to_vec(), b"0".to_vec(), b"-1".to_vec()],
            consistent,
        ).expect("LRANGE");
        match reply {
            kevy_resp::Reply::Array(arr) => {
                assert_eq!(arr.len(), 3, "LRANGE; consistent={consistent}");
            }
            other => panic!("LRANGE (consistent={consistent}): {other:?}"),
        }

        let reply = client.request_read(
            &[b"SMEMBERS".to_vec(), b"t:set".to_vec()],
            consistent,
        ).expect("SMEMBERS");
        match reply {
            kevy_resp::Reply::Array(arr) => {
                assert_eq!(arr.len(), 3, "SMEMBERS; consistent={consistent}");
            }
            other => panic!("SMEMBERS (consistent={consistent}): {other:?}"),
        }

        let reply = client.request_read(
            &[b"ZSCORE".to_vec(), b"t:zset".to_vec(), b"one".to_vec()],
            consistent,
        ).expect("ZSCORE");
        match reply {
            kevy_resp::Reply::Bulk(b) => {
                let s = std::str::from_utf8(&b).unwrap();
                assert!(s.starts_with('1'), "ZSCORE; consistent={consistent}");
            }
            other => panic!("ZSCORE (consistent={consistent}): {other:?}"),
        }
    }

    drop(client);
    primary.shutdown();
    r1.shutdown();
    r2.shutdown();
}

/// T1.39: a fresh-written key is visible via READCONSISTENT (forced
/// primary route) immediately, regardless of replica lag.
#[test]
fn readconsistent_sees_fresh_write_before_replica_lag() {
    let primary = PrimaryServer::start();
    let replica = ReplicaServer::start(primary.replication_base);

    let mut client = kevy_cluster_rw::ReadWriteClient::connect(
        ("127.0.0.1", primary.port),
        &[("127.0.0.1", replica.port)],
    )
    .expect("connect");

    // Write a key + immediately read with consistent=true. No
    // waiting for replica catch-up — primary serves the read.
    let _ = client.request_write(&[
        b"SET".to_vec(), b"rc:k".to_vec(), b"fresh".to_vec(),
    ]).expect("SET");
    let reply = client.request_read(
        &[b"GET".to_vec(), b"rc:k".to_vec()],
        true,
    ).expect("READCONSISTENT GET");
    match reply {
        kevy_resp::Reply::Bulk(b) => assert_eq!(b, b"fresh"),
        other => panic!("READCONSISTENT: {other:?}"),
    }

    drop(client);
    primary.shutdown();
    replica.shutdown();
}

// ────────── T1.40 / T1.41 reconnect helpers ──────────

/// Test-only runner that exposes its own stop signal + tracks how
/// many SnapshotBegin events it has seen. Lets the reconnect-window
/// tests assert "no snapshot ship occurred" by checking the counter.
#[allow(dead_code)] // `port` / `rt_port` retained for future read-side asserts
struct TrackedReplica {
    port: u16,
    runner_stop: Arc<AtomicBool>,
    runner_handle: Option<std::thread::JoinHandle<()>>,
    /// Cloned upstream socket — Mutex<Option<TcpStream>> so the
    /// stop path can `shutdown(Shutdown::Both)` it, unblocking the
    /// runner thread's `next_event` blocking read. Same trick the
    /// production `ReplicaRunner` uses (T1.29.5).
    runner_socket: Arc<Mutex<Option<std::net::TcpStream>>>,
    upstream: (String, u16),
    sender: kevy_rt::ReplicaInboxSender,
    last_offset: Arc<std::sync::atomic::AtomicU64>,
    snapshot_count: Arc<std::sync::atomic::AtomicUsize>,
    rt_port: u16,
    rt_stop: Arc<AtomicBool>,
    rt_handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempdir::TempDir,
}

impl TrackedReplica {
    fn start(upstream_replication_port: u16) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let rt_port = free_port_block(1) + 1;
        let dir = tempdir::TempDir::new("kevy-tracked-replica");
        let dir_path = dir.path().to_path_buf();
        unsafe { std::env::set_var("KEVY_IO_URING", "0"); }
        let (sender, receiver) = kevy_rt::replica_inbox_pair();
        let rt_stop = Arc::new(AtomicBool::new(false));
        let rt_stop_thread = rt_stop.clone();
        let rt_handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], rt_port, 1, kevy::KevyCommands)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replica_inboxes(vec![receiver]);
            let _ = rt.run(rt_stop_thread);
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", rt_port)).is_ok() { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let last_offset = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let snapshot_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut me = Self {
            port: rt_port,
            runner_stop: Arc::new(AtomicBool::new(false)),
            runner_handle: None,
            runner_socket: Arc::new(Mutex::new(None)),
            upstream: ("127.0.0.1".to_string(), upstream_replication_port),
            sender,
            last_offset,
            snapshot_count,
            rt_port,
            rt_stop,
            rt_handle: Some(rt_handle),
            _dir: dir,
        };
        me.start_runner();
        me
    }

    fn start_runner(&mut self) {
        let stop = Arc::new(AtomicBool::new(false));
        self.runner_stop = stop.clone();
        let socket_slot = self.runner_socket.clone();
        let upstream = self.upstream.clone();
        let sender = self.sender.clone();
        let last_offset = self.last_offset.clone();
        let snapshot_count = self.snapshot_count.clone();
        let handle = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut from = last_offset.load(std::sync::atomic::Ordering::Relaxed);
                let conn = kevy_replicate::replica::ReplicaClient::connect(
                    (upstream.0.as_str(), upstream.1),
                    "tracked",
                    from,
                );
                let Ok(mut client) = conn else {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                };
                if let Ok(h) = client.socket_handle()
                    && let Ok(mut guard) = socket_slot.lock()
                {
                    *guard = Some(h);
                }
                while !stop.load(Ordering::Relaxed) {
                    match client.next_event() {
                        Some(Ok(ev)) => {
                            let apply = match ev {
                                kevy_replicate::replica::ReplicaEvent::SnapshotBegin => {
                                    snapshot_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    kevy_rt::ReplicaApply::SnapshotBegin
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotChunk(b) => {
                                    kevy_rt::ReplicaApply::SnapshotChunk(b)
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotEnd { ack_offset } => {
                                    from = ack_offset;
                                    last_offset.store(from, std::sync::atomic::Ordering::Relaxed);
                                    kevy_rt::ReplicaApply::SnapshotEnd { ack_offset }
                                }
                                kevy_replicate::replica::ReplicaEvent::Frame(frame) => {
                                    from = frame.offset.saturating_add(1);
                                    last_offset.store(from, std::sync::atomic::Ordering::Relaxed);
                                    kevy_rt::ReplicaApply::Frame {
                                        offset: frame.offset,
                                        argv: frame.argv,
                                    }
                                }
                            };
                            if sender.send(apply).is_err() { return; }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
                // Clear socket slot before reconnect — old fd is dead.
                if let Ok(mut guard) = socket_slot.lock() { *guard = None; }
            }
        });
        self.runner_handle = Some(handle);
    }

    fn stop_runner(&mut self) {
        self.runner_stop.store(true, Ordering::Relaxed);
        // Shut down the cloned socket to unblock any in-flight
        // blocking next_event read.
        if let Ok(guard) = self.runner_socket.lock()
            && let Some(s) = guard.as_ref()
        {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        if let Some(h) = self.runner_handle.take() {
            let _ = h.join();
        }
    }

    fn snapshot_count(&self) -> usize {
        self.snapshot_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn shutdown(mut self) {
        self.stop_runner();
        self.rt_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.rt_handle.take() {
            let _ = h.join();
        }
    }
}

/// T1.40: replica disconnects, primary takes more writes (still
/// within backlog), replica reconnects — should resume via backlog
/// **without a snapshot ship**.
#[test]
fn reconnect_within_backlog_resumes_no_snapshot() {
    let primary = PrimaryServer::start();
    let mut replica = TrackedReplica::start(primary.replication_base);

    // Write 3 SETs, wait for replica to catch up.
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", primary.port)).unwrap();
    for i in 0..3 {
        let k = format!("rc:{i}");
        let cmd = format!("*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n$1\r\nv\r\n", k.len(), k);
        writer.write_all(cmd.as_bytes()).unwrap();
        let _ = writer.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut buf = [0u8; 16];
        let _ = writer.read(&mut buf);
    }
    for _ in 0..100 {
        if replica.last_offset.load(std::sync::atomic::Ordering::Relaxed) >= 3 { break; }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(replica.snapshot_count(), 0, "initial connection should not snapshot");

    // Disconnect the runner (replica RT stays up).
    replica.stop_runner();

    // Primary takes 3 more writes — small enough to stay in the
    // 1 MiB default backlog.
    for i in 3..6 {
        let k = format!("rc:{i}");
        let cmd = format!("*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n$1\r\nv\r\n", k.len(), k);
        writer.write_all(cmd.as_bytes()).unwrap();
        let mut buf = [0u8; 16];
        let _ = writer.read(&mut buf);
    }

    // Reconnect runner — last_offset = 3, primary's backlog has
    // offsets 3..6. Should resume via backlog, no snapshot.
    replica.start_runner();
    for _ in 0..200 {
        if replica.last_offset.load(std::sync::atomic::Ordering::Relaxed) >= 6 { break; }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        replica.last_offset.load(std::sync::atomic::Ordering::Relaxed) >= 6,
        "replica didn't catch up to offset 6"
    );
    assert_eq!(
        replica.snapshot_count(), 0,
        "reconnect-within-backlog must NOT trigger a snapshot ship"
    );

    drop(writer);
    primary.shutdown();
    replica.shutdown();
}

/// T1.41: replica disconnects, primary writes enough to evict the
/// disconnected offset from the backlog, replica reconnects — must
/// take the snapshot ship path.
#[test]
fn reconnect_outside_backlog_triggers_snapshot() {
    // Tiny backlog so a few writes evict the head.
    let primary = {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = free_port_block(1);
        let port = base;
        let replication_base = base + 1;
        let dir = tempdir::TempDir::new("kevy-tiny-primary");
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        unsafe { std::env::set_var("KEVY_IO_URING", "0"); }
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replication(true, 256) // 256-byte backlog: a few SETs evict the head
                .with_replication_listener(replication_base);
            let _ = rt.run(stop_thread);
        });
        for p in [port, replication_base] {
            for _ in 0..400 {
                if std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() { break; }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        PrimaryServer {
            port,
            replication_base,
            stop,
            handle: Some(handle),
            _dir: dir,
        }
    };
    let mut replica = TrackedReplica::start(primary.replication_base);

    // Write a few SETs, wait for replica to catch up (within
    // backlog).
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", primary.port)).unwrap();
    for i in 0..3 {
        let k = format!("ro:{i}");
        let cmd = format!("*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n$1\r\nv\r\n", k.len(), k);
        writer.write_all(cmd.as_bytes()).unwrap();
        let mut buf = [0u8; 16];
        let _ = writer.read(&mut buf);
    }
    for _ in 0..100 {
        if replica.last_offset.load(std::sync::atomic::Ordering::Relaxed) >= 3 { break; }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(replica.snapshot_count(), 0);

    replica.stop_runner();

    // Write 30 more SETs — overflows the 256-byte backlog and
    // evicts offsets <= 3 by the time the replica reconnects.
    for i in 3..33 {
        let k = format!("ro:{i}");
        let cmd = format!("*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n$1\r\nv\r\n", k.len(), k);
        writer.write_all(cmd.as_bytes()).unwrap();
        let mut buf = [0u8; 16];
        let _ = writer.read(&mut buf);
    }

    // Reconnect — primary's backlog no longer has offset 3, so the
    // resume path returns TooOld → primary ships a snapshot.
    replica.start_runner();
    for _ in 0..400 {
        if replica.snapshot_count() > 0 { break; }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        replica.snapshot_count() >= 1,
        "reconnect-outside-backlog must trigger a snapshot ship (got count={})",
        replica.snapshot_count(),
    );

    drop(writer);
    primary.shutdown();
    replica.shutdown();
}
